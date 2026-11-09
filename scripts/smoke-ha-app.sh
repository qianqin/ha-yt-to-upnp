#!/usr/bin/env bash
# Smoke test of the assembled Home Assistant app folder: checks the option
# validator against config.yaml (host, no Docker), builds the image for
# linux/amd64 the way the Supervisor does (docker build of the folder with
# BUILD_VERSION/BUILD_ARCH), runs it with test options and checks that all four
# services come up and stay up, run as their own users, listen where they
# should, get the options as environment and cannot read the Supervisor token;
# that the PoToken helper mints a token; and that an invalid option stops the
# app with one clear error.
#
# Nothing is announced on the LAN: the container runs on its own Docker bridge
# network (no host networking, no published ports), so the receiver's SSDP and
# the adapter's discovery stay inside it. The adapter finds no speaker there and
# must keep running anyway. The bridge network does NAT the container out to the
# internet: the image build needs it (Debian packages), and so does the PoToken
# check (POST /get_pot makes the helper fetch BotGuard from YouTube/Google
# through its restricted Deno permissions).
#
# Everything created here (buildx builder and its cache, image, containers,
# network, temp dir, a pulled buildkit image) is removed on exit.
set -euo pipefail

usage() { echo "usage: scripts/smoke-ha-app.sh [APP_DIR]  (default dist/ha-yt-to-upnp)" >&2; }

case "${1:-}" in -h|--help) usage; exit 0 ;; esac
[ $# -le 1 ] || { usage; exit 2; }

root="$(git rev-parse --show-toplevel)"
dir="${1:-$root/dist/ha-yt-to-upnp}"
die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

[ -f "$dir/artefacts/BUILD-INFO" ] || die "$dir is not an assembled app folder; run scripts/build-ha-app.sh first"
docker info >/dev/null 2>&1 || die "Docker daemon not running"

readonly PLATFORM=linux/amd64
readonly BUILDKIT_IMAGE=moby/buildkit:buildx-stable-1
readonly UP_TIMEOUT_S=180
readonly STABLE_S=20
version="$(sed -n 's/^version: "\(.*\)"$/\1/p' "$dir/config.yaml")"
[ -n "$version" ] || die "no version in $dir/config.yaml"

run_id="ytcast-smoke-$$"
image="ytcast-smoke:$$"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ytcast-smoke.XXXXXX")"
containers=""
network=""
builder=""
image_built=0
buildkit_pulled=0
docker image inspect "$BUILDKIT_IMAGE" >/dev/null 2>&1 || buildkit_pulled=1

cleanup() {
  status=$?
  set +e
  for c in $containers; do docker rm -fv "$c" >/dev/null 2>&1; done
  [ -n "$network" ] && docker network rm "$network" >/dev/null 2>&1
  [ "$image_built" = 1 ] && docker image rm "$image" >/dev/null 2>&1
  # Removing the builder also removes its container, state volume and cache.
  [ -n "$builder" ] && docker buildx rm --force "$builder" >/dev/null 2>&1
  [ "$buildkit_pulled" = 1 ] && docker image rm "$BUILDKIT_IMAGE" >/dev/null 2>&1
  rm -rf "$tmp"
  echo "==> cleanup: removed containers [${containers# }], network [$network], image [$image], builder [$builder]"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

step "option validator matches config.yaml's schema (host)"
python3 -I "$root/scripts/ha_options_test.py"

step "build $PLATFORM image from $dir (version $version)"
# A builder of its own keeps the build cache out of the default builder.
builder="$run_id"
docker buildx create --name "$builder" --driver docker-container >/dev/null
image_built=1
docker buildx build --builder "$builder" --platform "$PLATFORM" --load \
  --build-arg BUILD_VERSION="$version" --build-arg BUILD_ARCH=amd64 \
  -t "$image" "$dir"
docker image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' "$image"

network="$run_id"
docker network create "$network" >/dev/null

# Starts a container with the given options.json (copied in before the start,
# as the Supervisor provides /data/options.json).
start_app() {
  local name="$1" options="$2"
  containers="$containers $name"
  mkdir -p "$tmp/$name/data"
  printf '%s\n' "$options" > "$tmp/$name/data/options.json"
  # The Supervisor puts its token into every app's environment; the services
  # must not get it.
  docker create --name "$name" --platform "$PLATFORM" --network "$network" \
    -e SUPERVISOR_TOKEN=smoke-test-dummy-token "$image" >/dev/null
  docker cp "$tmp/$name/data" "$name:/data"
  docker start "$name" >/dev/null
}

# In-container checks, written in Python (the image has python3, not procps).
# Prints PASS/FAIL lines; exits 1 if any check fails.
cat > "$tmp/check.py" <<'PY'
import json, os, pwd, socket, subprocess, sys, time, urllib.error, urllib.request, wave

mode = sys.argv[1]
expect = json.loads(sys.argv[2])
failed = []

def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what)
    if not ok:
        failed.append(what)

# Mints a PoToken for a placeholder content binding: the helper fetches BotGuard
# from YouTube/Google within its --allow-net list. The token is not printed.
if mode == "pot":
    binding = "smoke-test-placeholder"
    request = urllib.request.Request("http://127.0.0.1:4416/get_pot",
                                     data=json.dumps({"content_binding": binding}).encode(),
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=240) as r:
            status, body = r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        status, body = e.code, json.loads(e.read() or b"{}")
    except Exception as e:
        status, body = repr(e), {}
    token = body.get("poToken") or ""
    check(status == 200 and len(token) > 20 and body.get("contentBinding") == binding,
          "PoToken helper mints a token on POST /get_pot (HTTP %s, %d chars%s)"
          % (status, len(token), ", error %r" % body["error"][:200] if "error" in body else ""))
    sys.exit(1 if failed else 0)

def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.connect(("192.0.2.1", 9))
    return s.getsockname()[0]

def can_connect(host, port):
    try:
        socket.create_connection((host, port), timeout=2).close()
        return True
    except OSError:
        return False

def procs():
    found = {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open("/proc/%s/cmdline" % pid, "rb") as f:
                argv = [a.decode() for a in f.read().split(b"\0") if a]
            # Under emulation on Apple Silicon the cmdline starts with the
            # emulator and the program's path: "rosetta /usr/bin/mpd mpd ...".
            if argv and os.path.basename(argv[0]).startswith(("rosetta", "qemu-")):
                argv = argv[2:]
            with open("/proc/%s/status" % pid) as f:
                uid = int(next(l for l in f if l.startswith("Uid:")).split()[1])
            with open("/proc/%s/environ" % pid, "rb") as f:
                env = dict(e.decode().split("=", 1) for e in f.read().split(b"\0") if b"=" in e)
        except (OSError, StopIteration):
            continue
        found[int(pid)] = (argv, uid, env)
    return found

def find(ps, needle):
    # A program by name (argv[0]) or a script by its path; s6-supervise's
    # argv carries the service name, so names never match later arguments.
    hits = [(pid, p) for pid, p in ps.items()
            if p[0] and (os.path.basename(p[0][0]) == needle
                         or (needle.startswith("/") and needle in p[0]))]
    return hits[0] if len(hits) == 1 else (None, None)

def user_of(uid):
    return pwd.getpwuid(uid).pw_name

def mpd(*commands):
    with socket.create_connection(("127.0.0.1", 6600), timeout=5) as s:
        f = s.makefile("rwb")
        f.readline()
        lines = []
        for c in commands:
            f.write((c + "\n").encode())
            f.flush()
            while True:
                line = f.readline().decode().rstrip("\n")
                lines.append(line)
                if line == "OK" or line.startswith("ACK"):
                    break
        return lines

# MPD opens its httpd output (port 8000) only once it plays, so play one second
# of generated silence from the music directory, repeated. The adapter then tries
# to take the speaker over, finds none on this network and must carry on.
def play_silence():
    path = "/data/mpd/music/smoke.wav"
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(44100)
        w.writeframes(b"\0" * 44100 * 4)
    owner = pwd.getpwnam("mpd")
    os.chown(path, owner.pw_uid, owner.pw_gid)
    mpd("update")
    for _ in range(20):
        if not any(l.startswith("updating_db") for l in mpd("status")):
            break
        time.sleep(0.5)
    return mpd("clear", "add smoke.wav", "repeat 1", "play")

ip = lan_ip()
ps = procs()
services = {"mpd": "mpd", "receiver": "ytcast-lite", "potoken": "deno",
            "adapter": "/opt/ytcast-adapter/raumfeld_adapter.py"}
users = {"mpd": "mpd", "receiver": "ytcast", "potoken": "potoken", "adapter": "adapter"}
pids = {}
for name, needle in services.items():
    pid, p = find(ps, needle)
    check(pid is not None, "%s runs (one process)" % name)
    if pid is None:
        continue
    pids[name] = pid
    check(user_of(p[1]) == users[name], "%s runs as %s, not root" % (name, users[name]))
    check("SUPERVISOR_TOKEN" not in p[2], "%s gets no SUPERVISOR_TOKEN" % name)

if mode == "full":
    try:
        with socket.create_connection(("127.0.0.1", 6600), timeout=3) as s:
            hello = s.recv(64).decode()
    except OSError as e:
        hello = str(e)
    check(hello.startswith("OK MPD"), "MPD answers on 127.0.0.1:6600 (%s)" % hello.strip())
    check(not can_connect(ip, 6600), "MPD control is not reachable on %s:6600" % ip)

    played = play_silence()
    check(all(l == "OK" for l in played), "MPD plays a test file (%s)" % played)
    status, ctype = None, ""
    for _ in range(20):
        try:
            r = urllib.request.urlopen("http://%s:8000/stream.mp3" % ip, timeout=5)
            ctype = r.headers.get("Content-Type", "")
            status = r.status
            r.close()
            break
        except Exception as e:
            status = repr(e)
            time.sleep(0.5)
    check(status == 200 and ctype == "audio/mpeg",
          "MPD httpd serves on %s:8000 (%s, %s)" % (ip, status, ctype))

    try:
        desc = urllib.request.urlopen("http://%s:8008/ssdp/device-desc.xml" % ip, timeout=5).read().decode()
    except Exception as e:
        desc = repr(e)
    check("<friendlyName>%s</friendlyName>" % expect["cast_name"] in desc,
          "receiver serves its DIAL device description on %s:8008 as %r" % (ip, expect["cast_name"]))

    try:
        pong = urllib.request.urlopen("http://127.0.0.1:4416/ping", timeout=5).read().decode()
    except Exception as e:
        pong = repr(e)
    check('"server_uptime"' in pong, "PoToken helper answers on 127.0.0.1:4416 (%s)" % pong[:80])
    check(not can_connect(ip, 4416), "PoToken helper is not reachable on %s:4416" % ip)

    st = os.stat("/data/ytcast/ytcast-lite.json") if os.path.exists("/data/ytcast/ytcast-lite.json") else None
    check(st is not None and st.st_mode & 0o077 == 0 and user_of(st.st_uid) == "ytcast",
          "receiver keeps its config private under /data/ytcast")

    # with-contenv keeps the container's environment in files; the services'
    # users must not be able to read the Supervisor token from there.
    envdir = "/run/s6/container_environment"
    token_file = envdir + "/SUPERVISOR_TOKEN"
    check(os.path.isfile(token_file) and os.stat(envdir).st_mode & 0o077 == 0,
          "%s holds SUPERVISOR_TOKEN and is private to root" % envdir)
    for user in sorted(set(users.values())):
        r = subprocess.run(["/command/s6-setuidgid", user, "cat", token_file],
                           stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        check(r.returncode != 0 and b"smoke-test-dummy-token" not in r.stdout,
              "%s cannot read %s (exit %d)" % (user, token_file, r.returncode))

if "receiver" in pids:
    env = ps[pids["receiver"]][2]
    for key, value in expect.get("receiver_env", {}).items():
        check(env.get(key) == value, "receiver env %s=%r (got %r)" % (key, value, env.get(key)))
if "potoken" in pids:
    env = ps[pids["potoken"]][2]
    allowed = {"DENO_DIR", "DENO_NO_UPDATE_CHECK", "NO_COLOR", "HOME", "PATH"}
    check(set(env) <= allowed, "PoToken helper environment is only %s (got %s)" % (sorted(allowed), sorted(env)))
if "adapter" in pids:
    env = ps[pids["adapter"]][2]
    for key, value in expect.get("adapter_env", {}).items():
        value = value.replace("{ip}", ip)
        check(env.get(key) == value, "adapter env %s=%r (got %r)" % (key, value, env.get(key)))
    for key in expect.get("adapter_env_absent", []):
        check(key not in env, "adapter env has no %s (adapter default)" % key)

print("PIDS " + json.dumps(pids, sort_keys=True))
sys.exit(1 if failed else 0)
PY

# Runs check.py until it passes or the timeout ends; prints the last run.
wait_for_checks() {
  local name="$1" mode="$2" expect="$3" deadline=$((SECONDS + UP_TIMEOUT_S))
  docker cp "$tmp/check.py" "$name:/tmp/check.py" >/dev/null
  # Privileged only for this exec: reading other users' /proc entries needs
  # CAP_SYS_PTRACE, which root in a container lacks.
  until docker exec --privileged "$name" python3 -I /tmp/check.py "$mode" "$expect" > "$tmp/$name.out" 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      cat "$tmp/$name.out"
      echo "--- container log ($name)"; docker logs "$name" 2>&1 | tail -60
      die "checks did not pass within ${UP_TIMEOUT_S}s"
    fi
    sleep 3
  done
  grep -v '^PIDS ' "$tmp/$name.out"
}

pids_of() { sed -n 's/^PIDS //p' "$tmp/$1.out"; }

step "run 1: all options set, stream host auto-detected"
main="$run_id-main"
start_app "$main" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
  "start_gate_seconds": 7, "stuck_seconds": 20, "log_level": "debug"}'
expect_main='{"cast_name": "Smoke Test",
  "receiver_env": {"DEVICE_NAME": "Smoke Test", "MPD_HOST": "127.0.0.1", "MPD_PORT": "6600",
                   "YTCAST_DATA_DIR": "/data/ytcast", "RUST_LOG": "info,ytcast_lite=debug"},
  "adapter_env": {"RAUMFELD_ROOM_UDN": "uuid:00000000-0000-0000-0000-000000000001",
                  "RAUMFELD_SPEAKER_UDN": "uuid:00000000-0000-0000-0000-000000000002",
                  "STREAM_URL": "http://{ip}:8000/stream.mp3", "MPD_HOST": "127.0.0.1",
                  "MPD_PORT": "6600", "START_GATE_SECONDS": "7", "STUCK_SECONDS": "20",
                  "LOG_LEVEL": "DEBUG"}}'
wait_for_checks "$main" full "$expect_main"
first_pids="$(pids_of "$main")"

step "run 1: PoToken helper mints a token (needs internet from the container)"
docker exec "$main" python3 -I /tmp/check.py pot '{}' || {
  echo "--- container log ($main)"; docker logs "$main" 2>&1 | tail -40
  die "the PoToken helper did not mint a token"
}

step "services stay up for ${STABLE_S}s (no restart loop)"
sleep "$STABLE_S"
wait_for_checks "$main" full "$expect_main" >/dev/null
[ "$(pids_of "$main")" = "$first_pids" ] || die "a service restarted: $first_pids -> $(pids_of "$main")"
echo "PASS no service restarted ($first_pids)"
logs="$(docker logs "$main" 2>&1)"
grep -q "Starting Raumfeld adapter" <<<"$logs" || die "adapter start not logged"
grep -q "DeviceNotFound" <<<"$logs" || die "adapter did not log the missing speaker"
echo "PASS adapter logged the missing speaker and kept running"
if grep -E "exited with code|exited \(code|Traceback" <<<"$logs"; then die "a service failed (see above)"; fi
echo "PASS no service exit or traceback in the log"
echo "--- container log excerpt (run 1)"
grep -E "Starting|\[init\]|SSDP|DIAL server|raumfeld_adapter|WARN|ERROR" <<<"$logs" | cut -c1-200 | head -40

step "run 2: explicit stream_host, optional timings unset"
second="$run_id-host"
start_app "$second" '{"cast_name": "Smoke Test 2",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
  "stream_host": "192.0.2.10", "log_level": "warning"}'
wait_for_checks "$second" env '{"receiver_env": {"DEVICE_NAME": "Smoke Test 2", "RUST_LOG": "warn"},
  "adapter_env": {"STREAM_URL": "http://192.0.2.10:8000/stream.mp3", "LOG_LEVEL": "WARNING"},
  "adapter_env_absent": ["START_GATE_SECONDS", "STUCK_SECONDS"]}'

step "run 3: an invalid option stops the app with one clear error"
bad="$run_id-bad"
start_app "$bad" '{"cast_name": "Bad <name>",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
  "log_level": "info"}'
deadline=$((SECONDS + 60))
until [ "$(docker inspect -f '{{.State.Status}}' "$bad")" = exited ]; do
  [ "$SECONDS" -lt "$deadline" ] || { docker logs "$bad" 2>&1 | tail -40; die "the app did not stop within 60s"; }
  sleep 2
done
bad_logs="$(docker logs "$bad" 2>&1)"
bad_code="$(docker inspect -f '{{.State.ExitCode}}' "$bad")"
echo "--- container log (run 3)"
cut -c1-200 <<<"$bad_logs"
[ "$bad_code" != 0 ] || die "the app stopped with exit code 0"
echo "PASS the app stopped (exit code $bad_code), no restart loop"
[ "$(grep -c "option 'cast_name' is invalid" <<<"$bad_logs")" = 1 ] || die "no single error naming cast_name"
grep -qF 'must match ^[^<>&"\x00-\x1f]{1,60}$' <<<"$bad_logs" || die "the error does not show the rule as written"
echo "PASS one error names the invalid option and shows its rule"
if grep -E "Starting " <<<"$bad_logs"; then die "a service started on invalid options"; fi
echo "PASS no service started"

step "smoke test passed"
