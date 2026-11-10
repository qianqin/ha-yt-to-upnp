#!/usr/bin/env bash
# Smoke test of the assembled Home Assistant app folder: checks the option
# validator against config.yaml (host, no Docker), builds the image for
# linux/amd64 the way the Supervisor does (docker build of the folder with
# BUILD_VERSION/BUILD_ARCH), runs it with test options and checks that all four
# services come up and stay up, run as their own users, listen where they
# should, get the options as environment and cannot read the Supervisor token;
# that the receiver's /silence answers on loopback only; that MPD's stream on
# port 8000 is FLAC 48 kHz 16-bit or MP3 320 kbit/s as stream_format says, under
# a URL that names it; that the PoToken helper mints a token; that the adapter
# keeps running whether it looks for the Raumfeld host by SSDP or is given an
# unreachable raumfeld_host; and that invalid options stop the app with one
# clear error.
#
# --quick: a check in seconds before a live test, not a replacement for the full
# run (which reviews need). One container with the default stream_format
# (flac): the four services up as their users, MPD answers, port 8000 serves
# FLAC while MPD plays, the DIAL description has the cast name, the PoToken
# helper answers /ping (it mints no token), the adapter gets its options; plus,
# in parallel, one invalid option that must stop the app. No stability wait.
# The image is built with the classic builder (no BuildKit cache is left) on a
# cached base image: the Dockerfile up to its "# smoke-quick-cache-boundary"
# line (base image, packages, PoToken helper), tagged
# ytcast-ha-smoke-base:<hash of that part and the files it copies>. A quick run
# makes it when there is none for the hash (replacing an older one) and keeps
# it; only the steps after the boundary run each time, on a build context
# without the files the base image already holds. --clean-cache removes it.
# Quick runs build the Dockerfile's last stage alone, with its
# COPY --from=artefacts /artefacts/... lines copying from artefacts/ in the
# build context instead.
#
# --release-path [TARBALL]: the build Home Assistant makes from the repository.
# The image is built with BuildKit from ha-yt-to-upnp/ as committed (no
# artefacts/ folder), so its artefacts stage downloads the release tarball
# (default dist/release/<asset of config.yaml's version>, made by
# scripts/build-ha-app.sh --release) and checks it against artefacts.lock. The
# tarball is served over http on 127.0.0.1 of this machine and reached as
# host.docker.internal (Docker Desktop), through the test-only ARTEFACTS_URL
# build argument. Before that, a tampered copy of the tarball and a
# BUILD_VERSION other than the lock's must each fail the build. The image then
# gets the quick run's checks, and its programs must be the tarball's
# (BUILD-INFO and checksums).
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
# network, temp dir, a pulled buildkit or base image) is removed on exit; with
# --quick, all of it but the cached base image.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/smoke-ha-app.sh [--quick] [APP_DIR]  (default dist/ha-yt-to-upnp)
       scripts/smoke-ha-app.sh --release-path [TARBALL]
                                  (default dist/release/<asset of the app version>)
       scripts/smoke-ha-app.sh --clean-cache        (remove --quick's base image)
USAGE
}

readonly BASE_REPO=ytcast-ha-smoke-base
readonly BASE_LABEL=io.github.ytcast.smoke-base
die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

# Removes an image and then its untagged parents, child first: the classic
# builder leaves one per step, and with the containerd image store removing an
# image leaves them. Stops at a tagged parent (the cached base image, or a base
# image that was there before) or one that another image still uses. An image
# loaded from BuildKit has no Parent field at all (not an empty one).
rm_image_chain() {
  local id="$1" parent
  while :; do
    parent="$(docker image inspect -f '{{with index . "Parent"}}{{.}}{{end}}' "$id" 2>/dev/null)" || return 0
    docker image rm "$id" >/dev/null 2>&1 || return 0
    [ -n "$parent" ] || return 0
    [ "$(docker image inspect -f '{{len .RepoTags}}' "$parent" 2>/dev/null)" = 0 ] || return 0
    id="$parent"
  done
}

mode=full
case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  --clean-cache)
    [ $# -eq 1 ] || { usage; exit 2; }
    docker info >/dev/null 2>&1 || die "Docker daemon not running"
    cached="$(docker image ls --filter "label=$BASE_LABEL" --format '{{.Repository}}:{{.Tag}}')"
    for ref in $cached; do rm_image_chain "$ref"; echo "removed $ref"; done
    [ -n "$cached" ] || echo "no cached base image"
    exit 0 ;;
  --quick) mode=quick; shift ;;
  --release-path) mode=release; shift ;;
esac
[ $# -le 1 ] || { usage; exit 2; }

root="$(git rev-parse --show-toplevel)"
if [ "$mode" = release ]; then
  # The folder as Home Assistant takes it from the repository.
  dir="$root/ha-yt-to-upnp"
  if [ -n "$(git -C "$root" status --porcelain --untracked-files=all --ignored -- ha-yt-to-upnp)" ]; then
    git -C "$root" status --short --untracked-files=all --ignored -- ha-yt-to-upnp >&2
    die "ha-yt-to-upnp/ differs from the commit (or holds ignored files); commit or remove that first"
  fi
else
  dir="${1:-$root/dist/ha-yt-to-upnp}"
  [ -f "$dir/artefacts/BUILD-INFO" ] || die "$dir is not an assembled app folder; run scripts/build-ha-app.sh first"
fi
docker info >/dev/null 2>&1 || die "Docker daemon not running"

readonly PLATFORM=linux/amd64
readonly BUILDKIT_IMAGE=moby/buildkit:buildx-stable-1
readonly STABLE_S=20
if [ "$mode" != full ]; then
  readonly UP_TIMEOUT_S=60 POLL_S=0.5
else
  readonly UP_TIMEOUT_S=180 POLL_S=3
fi
version="$(sed -n 's/^version: "\(.*\)"$/\1/p' "$dir/config.yaml")"
[ -n "$version" ] || die "no version in $dir/config.yaml"
if [ "$mode" = release ]; then
  asset="ha-yt-to-upnp-artefacts-$version-amd64.tar.gz"
  tarball="${1:-$root/dist/release/$asset}"
  [ -f "$tarball" ] || die "no release tarball $tarball; run scripts/build-ha-app.sh --release first"
fi

run_id="ytcast-smoke-$$"
image="ytcast-smoke:$$"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ytcast-smoke.XXXXXX")"
containers=""
network=""
builder=""
image_built=0
buildkit_pulled=0
from_pulled=""
server_pid=""

cleanup() {
  status=$?
  set +e
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null
  for c in $containers; do docker rm -fv "$c" >/dev/null 2>&1; done
  [ -n "$network" ] && docker network rm "$network" >/dev/null 2>&1
  [ "$image_built" = 1 ] && rm_image_chain "$image"
  # Removing the builder also removes its container, state volume and cache.
  [ -n "$builder" ] && docker buildx rm --force "$builder" >/dev/null 2>&1
  [ "$buildkit_pulled" = 1 ] && docker image rm "$BUILDKIT_IMAGE" >/dev/null 2>&1
  [ -n "$from_pulled" ] && docker image rm "$from_pulled" >/dev/null 2>&1
  rm -rf "$tmp"
  echo "==> cleanup: removed containers [${containers# }], network [$network], image [$image], builder [$builder]"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

step "option validator matches config.yaml's schema (host)"
python3 -I "$root/scripts/ha_options_test.py"

# The full run builds as the Supervisor does: BuildKit, on a builder of its own
# so that the build cache stays out of the default builder.
create_builder() {
  docker image inspect "$BUILDKIT_IMAGE" >/dev/null 2>&1 || buildkit_pulled=1
  builder="$run_id"
  docker buildx create --name "$builder" --driver docker-container >/dev/null
}

build_full() {
  create_builder
  image_built=1
  docker buildx build --builder "$builder" --platform "$PLATFORM" --load \
    --build-arg BUILD_VERSION="$version" --build-arg BUILD_ARCH=amd64 \
    -t "$image" "$dir"
}

# The quick run splits the Dockerfile at its boundary line: the part above
# becomes the cached base image, named by the hash of that part and of the
# files it copies, and the rest is built on it. The classic builder keeps no
# build cache: removing the image removes all it made.
build_quick() {
  local last split sources base from old
  last="$(grep -n '^FROM ' "$dir/Dockerfile" | tail -n 1 | cut -d: -f1)"
  tail -n "+$last" "$dir/Dockerfile" \
    | sed 's|^COPY --from=artefacts /artefacts/|COPY artefacts/|' > "$tmp/Dockerfile.last"
  if grep -q -- '--from=' "$tmp/Dockerfile.last"; then die "unexpected COPY --from in the last stage"; fi
  split="$(grep -n '^# smoke-quick-cache-boundary$' "$tmp/Dockerfile.last" | cut -d: -f1)"
  [ -n "$split" ] || die "no '# smoke-quick-cache-boundary' line in $dir/Dockerfile"
  head -n "$split" "$tmp/Dockerfile.last" > "$tmp/Dockerfile.base"
  # COPY's sources in the base part (the last word is the destination).
  sources="$(awk '/^COPY /{for (i = 2; i < NF; i++) print $i}' "$tmp/Dockerfile.base")"
  base="$BASE_REPO:$( { cat "$tmp/Dockerfile.base"
    # shellcheck disable=SC2086  # one path per word
    (cd "$dir" && find $sources -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256)
  } | shasum -a 256 | cut -c1-16)"
  if docker image inspect "$base" >/dev/null 2>&1; then
    echo "cached base image $base"
  else
    from="$(sed -n 's/^FROM \([^ ]*\).*/\1/p' "$tmp/Dockerfile.base" | head -n 1)"
    docker image inspect "$from" >/dev/null 2>&1 || from_pulled="$from"
    old="$(docker image ls --filter "label=$BASE_LABEL" --format '{{.Repository}}:{{.Tag}}')"
    echo "building base image $base (kept for later quick runs; --clean-cache removes it)"
    DOCKER_BUILDKIT=0 docker build -q --platform "$PLATFORM" --label "$BASE_LABEL=1" \
      -t "$base" -f "$tmp/Dockerfile.base" "$dir" >/dev/null
    # The base image holds the layers; the pulled image's name is not needed.
    if [ -n "$from_pulled" ]; then docker image rm "$from_pulled" >/dev/null; from_pulled=""; fi
    for ref in $old; do rm_image_chain "$ref"; echo "removed older $ref"; done
  fi
  { echo "FROM $base"; tail -n "+$((split + 1))" "$tmp/Dockerfile.last"; } > "$tmp/Dockerfile.quick"
  # The classic builder sends the whole context: leave out what the base holds.
  mkdir "$tmp/context"
  (cd "$dir" && find . \( -type f -o -type l \) -print) \
    | SKIP="$sources" awk 'BEGIN { n = split(ENVIRON["SKIP"], skip, "\n") }
        { for (i = 1; i <= n; i++) if (index($0, "./" skip[i]) == 1) next; print }' \
    | tar -cf - -C "$dir" -T - | tar -xpf - -C "$tmp/context"
  image_built=1
  DOCKER_BUILDKIT=0 docker build -q --platform "$PLATFORM" \
    --build-arg BUILD_VERSION="$version" --build-arg BUILD_ARCH=amd64 \
    -t "$image" -f "$tmp/Dockerfile.quick" "$tmp/context" >/dev/null
}

# Builds only the artefacts stage with the given ARTEFACTS_URL and
# BUILD_VERSION; the build must fail with a line containing the message.
expect_artefacts_failure() {
  local what="$1" url="$2" build_version="$3" message="$4" log="$tmp/build-$1.log"
  if docker buildx build --builder "$builder" --platform "$PLATFORM" --no-cache --progress plain \
      --target artefacts --build-arg BUILD_VERSION="$build_version" --build-arg BUILD_ARCH=amd64 \
      --build-arg ARTEFACTS_URL="$url" "$dir" > "$log" 2>&1; then
    tail -20 "$log"; die "the build with $what did not fail"
  fi
  grep -F "fetch-artefacts: error: $message" "$log" || { tail -20 "$log"; die "the build with $what failed, but not with: $message"; }
  echo "PASS the build with $what fails: $message"
}

# The release path: the tarball is served over http on 127.0.0.1, which
# containers on Docker Desktop reach as host.docker.internal.
build_release() {
  local lock_sha actual port url deadline
  lock_sha="$(sed -n 's/^sha256: //p' "$dir/artefacts.lock")"
  actual="$(shasum -a 256 "$tarball" | cut -d' ' -f1)"
  [ "$actual" = "$lock_sha" ] || die "$tarball (sha256 $actual) is not the one in artefacts.lock ($lock_sha)"
  echo "tarball $tarball, sha256 $actual (as in artefacts.lock)"
  mkdir -p "$tmp/srv/genuine" "$tmp/srv/tampered"
  cp "$tarball" "$tmp/srv/genuine/$asset"
  # Still a valid gzip and tar file: gzip reads the appended member as more data.
  cp "$tarball" "$tmp/srv/tampered/$asset"
  printf 'tampered\n' | gzip -n >> "$tmp/srv/tampered/$asset"
  python3 -I -u -m http.server 0 --bind 127.0.0.1 --directory "$tmp/srv" > "$tmp/server.log" 2>&1 &
  server_pid=$!
  deadline=$((SECONDS + 10))
  until port="$(sed -n 's/^Serving HTTP on 127.0.0.1 port \([0-9]*\).*/\1/p' "$tmp/server.log")" && [ -n "$port" ]; do
    [ "$SECONDS" -lt "$deadline" ] || { cat "$tmp/server.log"; die "the http server did not start"; }
    sleep 0.2
  done
  url="http://host.docker.internal:$port"
  create_builder
  expect_artefacts_failure "a tampered tarball" "$url/tampered/$asset" "$version" "sha256 mismatch"
  expect_artefacts_failure "BUILD_VERSION 0.0.0" "$url/genuine/$asset" 0.0.0 \
    "artefacts.lock is for version $version, the app is 0.0.0"
  image_built=1
  docker buildx build --builder "$builder" --platform "$PLATFORM" --load --progress plain \
    --build-arg BUILD_VERSION="$version" --build-arg BUILD_ARCH=amd64 \
    --build-arg ARTEFACTS_URL="$url/genuine/$asset" -t "$image" "$dir" > "$tmp/build-genuine.log" 2>&1 \
    || { tail -40 "$tmp/build-genuine.log"; die "the build from the release tarball failed"; }
  grep -F "fetch-artefacts: sha256 $actual matches artefacts.lock" "$tmp/build-genuine.log"
  echo "PASS the build downloads the release tarball and checks its sha256"
}

step "build $PLATFORM image from $dir (version $version, $mode)"
case "$mode" in
  quick) build_quick ;;
  release) build_release ;;
  full) build_full ;;
esac
docker image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' "$image"

network="$run_id"
docker network create "$network" >/dev/null

# Starts a container with the given options.json (copied in before the start,
# as the Supervisor provides /data/options.json). An optional third argument
# names an entry under /data to put there as a symbolic link to /tmp.
start_app() {
  local name="$1" options="$2" link="${3:-}"
  containers="$containers $name"
  mkdir -p "$tmp/$name/data"
  printf '%s\n' "$options" > "$tmp/$name/data/options.json"
  [ -z "$link" ] || ln -s /tmp "$tmp/$name/data/$link"
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
import json, os, pwd, socket, stat, subprocess, sys, time, urllib.error, urllib.request, wave

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

# What the start of a stream says about itself: FLAC's STREAMINFO block, or the
# first MP3 frame header (after an ID3v2 tag, if there is one).
def stream_params(head):
    if head[:4] == b"fLaC":
        if len(head) < 42 or head[4] & 0x7f != 0:
            return {"codec": "flac", "error": "no STREAMINFO first"}
        si = head[8:42]
        return {"codec": "flac", "rate": si[10] << 12 | si[11] << 4 | si[12] >> 4,
                "channels": (si[12] >> 1 & 7) + 1, "bits": ((si[12] & 1) << 4 | si[13] >> 4) + 1}
    start = 0
    if head[:3] == b"ID3" and len(head) >= 10:
        start = 10 + (head[6] << 21 | head[7] << 14 | head[8] << 7 | head[9])
    for i in range(start, len(head) - 3):
        if head[i] == 0xff and head[i + 1] & 0xe0 == 0xe0:
            h1, h2, h3 = head[i + 1], head[i + 2], head[i + 3]
            if h1 & 0x1e != 0x1a:  # MPEG-1 Layer III
                return {"codec": "mpeg", "error": "not MPEG-1 Layer III (0x%02x)" % h1}
            return {"codec": "mp3", "kbit": [None, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160,
                                             192, 224, 256, 320, None][h2 >> 4],
                    "rate": [44100, 48000, 32000, None][h2 >> 2 & 3],
                    "channels": 1 if h3 >> 6 == 3 else 2}
    return {"codec": None, "error": "unknown start %r" % head[:8]}

# The stream as the speaker gets it, from the URL the adapter hands it (the
# adapter's STREAM_URL may name another host; MPD listens on all of them).
def check_stream(want):
    played = play_silence()
    check(all(l == "OK" for l in played), "MPD plays a test file (%s)" % played)
    url = "http://%s:8000%s" % (ip, want["path"])
    status, ctype, head = None, "", b""
    for _ in range(20):
        try:
            with urllib.request.urlopen(url, timeout=5) as r:
                status, ctype = r.status, r.headers.get("Content-Type", "")
                # Only as much as the header needs: MPD sends in real time, and
                # FLAC packs silence into a few bytes a frame.
                while len(head) < 8192 and "error" in stream_params(head):
                    chunk = r.read1(4096)
                    if not chunk:
                        break
                    head += chunk
            break
        except Exception as e:
            status = repr(e)
            time.sleep(0.5)
    check(status == 200 and ctype == want["mime"],
          "MPD httpd serves %s (%s, %s)" % (url, status, ctype))
    params = stream_params(head)
    check(params == want["params"], "stream is %s (got %s)" % (want["params"], params))

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

if mode in ("full", "quick"):
    try:
        with socket.create_connection(("127.0.0.1", 6600), timeout=3) as s:
            hello = s.recv(64).decode()
    except OSError as e:
        hello = str(e)
    check(hello.startswith("OK MPD"), "MPD answers on 127.0.0.1:6600 (%s)" % hello.strip())

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

if mode == "full":
    check(not can_connect(ip, 6600), "MPD control is not reachable on %s:6600" % ip)
    check(not can_connect(ip, 4416), "PoToken helper is not reachable on %s:4416" % ip)

    st = os.stat("/data/ytcast/ytcast-lite.json") if os.path.exists("/data/ytcast/ytcast-lite.json") else None
    check(st is not None and st.st_mode & 0o077 == 0 and user_of(st.st_uid) == "ytcast",
          "receiver keeps its config private under /data/ytcast")

    # The pre-roll's endless silence, fetched by MPD at http://localhost:8008/silence.
    # It is not paced, so curl reads it at a capped rate and ends it after 2 s
    # (exit 28); what arrived must be zero samples.
    def silence(host):
        r = subprocess.run(["curl", "-sS", "--max-time", "2", "--limit-rate", "256k",
                            "-o", "/tmp/silence.raw",
                            "-w", "%{http_code} %{content_type}",
                            "http://%s:8008/silence" % host],
                           stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            with open("/tmp/silence.raw", "rb") as f:
                body = f.read()
            os.remove("/tmp/silence.raw")
        except OSError:
            body = b""
        return r.stdout, body
    answer, body = silence("localhost")
    check(answer == "200 audio/L16;rate=44100;channels=2" and len(body) > 4096
          and body.count(0) == len(body),
          "receiver serves /silence on loopback (%s, %d zero bytes)" % (answer, len(body)))
    answer, body = silence(ip)
    check(answer.startswith("403") and not body,
          "receiver refuses /silence on %s:8008 (%s)" % (ip, answer))

    for path, owner in (("/data/adapter", "adapter"), ("/data/ytcast", "ytcast"),
                        ("/data/mpd", "mpd"), ("/data/mpd/music", "mpd"),
                        ("/data/mpd/playlists", "mpd")):
        st = os.lstat(path) if os.path.lexists(path) else None
        ok = st is not None and stat.S_ISDIR(st.st_mode)
        check(ok and st.st_mode & 0o777 == 0o700 and user_of(st.st_uid) == owner,
              "%s is a directory, mode 0700, owned by %s (%s)"
              % (path, owner, (oct(st.st_mode & 0o777), user_of(st.st_uid)) if ok else "missing"))

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

if "stream" in expect and "mpd" in pids:
    check_stream(expect["stream"])

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
    sleep "$POLL_S"
  done
  grep -v '^PIDS ' "$tmp/$name.out"
}

pids_of() { sed -n 's/^PIDS //p' "$tmp/$1.out"; }

# Waits until the container has stopped; it must stop with an error, one FATAL
# line with the message and no service started.
expect_stopped() {
  local name="$1" message="$2" deadline=$((SECONDS + 60)) logs code
  until [ "$(docker inspect -f '{{.State.Status}}' "$name")" = exited ]; do
    [ "$SECONDS" -lt "$deadline" ] || { docker logs "$name" 2>&1 | tail -40; die "the app did not stop within 60s"; }
    sleep "$POLL_S"
  done
  logs="$(docker logs "$name" 2>&1)"
  code="$(docker inspect -f '{{.State.ExitCode}}' "$name")"
  echo "--- container log ($name)"
  cut -c1-200 <<<"$logs"
  [ "$code" != 0 ] || die "the app stopped with exit code 0"
  echo "PASS the app stopped (exit code $code), no restart loop"
  [ "$(grep -F "$message" <<<"$logs" | grep -c FATAL)" = 1 ] || die "no single FATAL line with: $message"
  echo "PASS one FATAL line: $message"
  if grep -E "Starting " <<<"$logs"; then die "a service started on invalid options"; fi
  echo "PASS no service started"
}

if [ "$mode" != full ]; then
  step "quick run: default options (flac), raumfeld_host given; an invalid stream_format in parallel"
  started=$SECONDS
  start_app "$run_id-quick" '{"cast_name": "Smoke Test",
    "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
    "raumfeld_host": "192.0.2.20", "stream_format": "flac", "log_level": "info"}'
  start_app "$run_id-badformat" '{"cast_name": "Smoke Test",
    "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
    "raumfeld_host": "192.0.2.20", "stream_format": "flac24", "log_level": "info"}'
  wait_for_checks "$run_id-quick" quick '{"cast_name": "Smoke Test",
    "stream": {"path": "/stream.flac", "mime": "audio/flac",
               "params": {"codec": "flac", "rate": 48000, "channels": 2, "bits": 16}},
    "adapter_env": {"RAUMFELD_HOST": "192.0.2.20", "STREAM_MIME": "audio/flac",
                    "STREAM_URL": "http://{ip}:8000/stream.flac",
                    "RAUMFELD_ROOM_UDN": "uuid:00000000-0000-0000-0000-000000000001"},
    "adapter_env_absent": ["RAUMFELD_SPEAKER_UDN"]}'
  echo "services up and checked $((SECONDS - started))s after the start"
  expect_stopped "$run_id-badformat" "option 'stream_format' is invalid: must be one of flac, mp3"
  if [ "$mode" = release ]; then
    step "release path: the image's programs are the tarball's"
    expected_info="$(tar -xzOf "$tarball" artefacts/BUILD-INFO)"
    [ "$(docker exec "$run_id-quick" cat /opt/BUILD-INFO)" = "$expected_info" ] || die "/opt/BUILD-INFO is not the tarball's"
    echo "PASS /opt/BUILD-INFO is the tarball's"
    for pair in "receiver sha256=/usr/local/bin/ytcast-lite" \
        "adapter sha256=/opt/ytcast-adapter/raumfeld_adapter.py" "deno binary sha256=/usr/local/bin/deno"; do
      want="$(sed -n "s/^${pair%%=*}: //p" <<<"$expected_info")"
      got="$(docker exec "$run_id-quick" sha256sum "${pair#*=}" | cut -d' ' -f1)"
      [ -n "$want" ] && [ "$got" = "$want" ] || die "${pair#*=}: sha256 $got, BUILD-INFO says $want"
      echo "PASS ${pair#*=} has the ${pair%%=*} from BUILD-INFO ($got)"
    done
  fi
  step "quick smoke test passed ($((SECONDS - started))s after the start; the full run is still needed for review)"
  exit 0
fi

step "run 1: all options set, Raumfeld host by SSDP, stream host auto-detected, FLAC stream (default)"
main="$run_id-main"
start_app "$main" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
  "start_gate_seconds": 7, "stuck_seconds": 20, "stream_format": "flac", "log_level": "debug"}'
expect_main='{"cast_name": "Smoke Test",
  "stream": {"path": "/stream.flac", "mime": "audio/flac",
             "params": {"codec": "flac", "rate": 48000, "channels": 2, "bits": 16}},
  "receiver_env": {"DEVICE_NAME": "Smoke Test", "MPD_HOST": "127.0.0.1", "MPD_PORT": "6600",
                   "YTCAST_DATA_DIR": "/data/ytcast", "RUST_LOG": "info,ytcast_lite=debug"},
  "adapter_env": {"RAUMFELD_ROOM_UDN": "uuid:00000000-0000-0000-0000-000000000001",
                  "RAUMFELD_SPEAKER_UDN": "uuid:00000000-0000-0000-0000-000000000002",
                  "STREAM_URL": "http://{ip}:8000/stream.flac", "STREAM_MIME": "audio/flac",
                  "MPD_HOST": "127.0.0.1",
                  "MPD_PORT": "6600", "START_GATE_SECONDS": "7", "STUCK_SECONDS": "20",
                  "LOG_LEVEL": "DEBUG", "ADAPTER_STATE_DIR": "/data/adapter"},
  "adapter_env_absent": ["RAUMFELD_HOST"]}'
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

# Waits until the container's log has a line matching the regex.
wait_for_log() {
  local name="$1" pattern="$2" deadline=$((SECONDS + 90))
  until docker logs "$name" 2>&1 | grep -qE "$pattern"; do
    [ "$SECONDS" -lt "$deadline" ] || { docker logs "$name" 2>&1 | tail -40; die "no log line matching '$pattern' within 90s"; }
    sleep 2
  done
}

step "run 2: raumfeld_host (unreachable) without speaker UDN, explicit stream_host, optional timings unset, MP3 stream"
second="$run_id-host"
start_app "$second" '{"cast_name": "Smoke Test 2",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_host": "192.0.2.20",
  "stream_host": "192.0.2.10", "stream_format": "mp3", "log_level": "warning"}'
expect_host='{"receiver_env": {"DEVICE_NAME": "Smoke Test 2", "RUST_LOG": "warn"},
  "stream": {"path": "/stream.mp3", "mime": "audio/mpeg",
             "params": {"codec": "mp3", "kbit": 320, "rate": 48000, "channels": 2}},
  "adapter_env": {"RAUMFELD_HOST": "192.0.2.20", "ADAPTER_STATE_DIR": "/data/adapter",
                  "RAUMFELD_ROOM_UDN": "uuid:00000000-0000-0000-0000-000000000001",
                  "STREAM_URL": "http://192.0.2.10:8000/stream.mp3", "STREAM_MIME": "audio/mpeg",
                  "LOG_LEVEL": "WARNING"},
  "adapter_env_absent": ["RAUMFELD_SPEAKER_UDN", "START_GATE_SECONDS", "STUCK_SECONDS"]}'
wait_for_checks "$second" env "$expect_host"
host_pids="$(pids_of "$second")"
# The adapter's start-up look for the room fails (no answer from 192.0.2.20);
# it logs that and waits for the first cast.
wait_for_log "$second" "Raumfeld room not found yet"
sleep "$STABLE_S"
wait_for_checks "$second" env "$expect_host" >/dev/null
[ "$(pids_of "$second")" = "$host_pids" ] || die "a service restarted: $host_pids -> $(pids_of "$second")"
echo "PASS no service restarted ($host_pids)"
host_logs="$(docker logs "$second" 2>&1)"
grep -q "Starting Raumfeld adapter (stream http://192.0.2.10:8000/stream.mp3, Raumfeld host 192.0.2.20)" <<<"$host_logs" \
  || die "adapter start with the Raumfeld host not logged"
echo "PASS adapter start logged with the Raumfeld host"
echo "PASS adapter logged the unreachable host and kept running:"
grep "Raumfeld room not found yet" <<<"$host_logs" | cut -c1-240
if grep -E "exited with code|exited \(code|Traceback|configuration error" <<<"$host_logs"; then die "a service failed (see above)"; fi
echo "PASS no service exit, configuration error or traceback in the log"

# Starts the app with invalid options; it must stop (no restart loop) with one
# FATAL line naming the option, before any service starts.
expect_fatal() {
  local name="$1" options="$2" message="$3" link="${4:-}"
  start_app "$name" "$options" "$link"
  expect_stopped "$name" "$message"
}

step "run 3: an invalid cast_name stops the app with one clear error"
expect_fatal "$run_id-bad" '{"cast_name": "Bad <name>",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
  "stream_format": "flac", "log_level": "info"}' "option 'cast_name' is invalid"
grep -qF 'must match ^[^<>&"\x00-\x1f]{1,60}$' <<<"$(docker logs "$run_id-bad" 2>&1)" \
  || die "the error does not show the rule as written"
echo "PASS the error shows the rule as written"

step "run 4: an invalid raumfeld_host stops the app with one clear error"
expect_fatal "$run_id-badhost" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_host": "192.0.2.20:47365", "stream_format": "flac", "log_level": "info"}' "option 'raumfeld_host' is invalid"

step "run 5: neither raumfeld_speaker_udn nor raumfeld_host stops the app with one clear error"
expect_fatal "$run_id-nohost" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "stream_format": "flac", "log_level": "info"}' "option 'raumfeld_speaker_udn' is required when raumfeld_host is not set"

step "run 6: a symbolic link at /data/adapter stops the app with one clear error"
expect_fatal "$run_id-link" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_host": "192.0.2.20", "stream_format": "flac", "log_level": "info"}' \
  "/data/adapter is a symbolic link; remove it and start the app again" adapter
# The link's target in the stopped container must be untouched: still root's
# world-writable sticky /tmp, not the adapter's 0700 directory.
tmp_entry="$(docker cp "$run_id-link:/tmp" - | python3 -I -c 'import io, sys, tarfile
# Read it all, so docker cp never writes into a closed pipe.
m = tarfile.open(fileobj=io.BytesIO(sys.stdin.buffer.read())).next()
print("mode %o uid %d gid %d" % (m.mode, m.uid, m.gid))')"
[ "$tmp_entry" = "mode 1777 uid 0 gid 0" ] || die "the link's target /tmp was changed: $tmp_entry"
echo "PASS the link's target was left alone (/tmp: $tmp_entry)"

step "run 7: an invalid stream_format stops the app with one clear error"
expect_fatal "$run_id-badformat" '{"cast_name": "Smoke Test",
  "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
  "raumfeld_host": "192.0.2.20", "stream_format": "flac24", "log_level": "info"}' \
  "option 'stream_format' is invalid: must be one of flac, mp3"

step "smoke test passed"
