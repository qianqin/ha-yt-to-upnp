#!/usr/bin/env bash
# Runs the Home Assistant app's four processes natively on this Mac, for a live
# test with a phone and a Raumfeld speaker on the LAN, configured the way the
# app is: the app's options (checked by the app's own check), MPD with the app's
# configuration template and stream_format encoder, the PoToken helper and the
# adapter from the assembled app folder, and the receiver built from this
# checkout for macOS.
#
# Why not the app's image in Docker: on Docker Desktop for Mac even
# --network host is the Linux VM's network (192.168.65.x), not the Mac's LAN.
# The phone's SSDP search never reaches the receiver, the receiver would
# announce an address the phone cannot reach, and the adapter's SSDP search for
# the zone renderer never reaches the speakers.
#
# The processes run in the background as the current user; their state and logs
# are in one directory, removed by --stop. Ports on this Mac: 8008/tcp (DIAL)
# and 1900/udp (SSDP) on the LAN, 8000/tcp (stream) on the LAN, 6600 (MPD) and
# 4416 (PoToken helper) on loopback.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/run-ha-app-local.sh --name NAME --room-udn UDN
           (--speaker-udn UDN | --raumfeld-host HOST) [--speaker-udn UDN]
           [--stream-format flac|mp3] [--stream-host HOST] [--log-level LEVEL]
           [--start-gate-seconds N] [--stuck-seconds N] [--app-dir DIR]
       scripts/run-ha-app-local.sh --stop
  --app-dir DIR  assembled app folder (default dist/ha-yt-to-upnp)
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

root="$(git rev-parse --show-toplevel)"
run="${TMPDIR:-/tmp}/ytcast-local-$(id -u)"
run="${run//\/\//\/}"

# Stops the processes listed in $run/pids (TERM, then KILL after 5 s) and
# removes $run.
stop() {
  [ -d "$run" ] || { echo "nothing running ($run does not exist)"; return 0; }
  local pid name deadline
  if [ -f "$run/pids" ]; then
    while read -r name pid; do
      kill "$pid" 2>/dev/null && echo "stopping $name ($pid)"
    done < "$run/pids"
    deadline=$((SECONDS + 5))
    while read -r name pid; do
      while kill -0 "$pid" 2>/dev/null; do
        if [ "$SECONDS" -ge "$deadline" ]; then kill -9 "$pid" 2>/dev/null; break; fi
        sleep 0.2
      done
    done < "$run/pids"
  fi
  rm -rf "$run"
  echo "stopped; removed $run"
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  --stop) [ $# -eq 1 ] || { usage; exit 2; }; stop; exit 0 ;;
esac

app="$root/dist/ha-yt-to-upnp"
# Options as the Supervisor writes them: defaults from config.yaml, unset
# optional options left out.
opts=(stream_format flac log_level info)
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || { usage; exit 2; }
  case "$1" in
    --name) opts+=(cast_name "$2") ;;
    --room-udn) opts+=(raumfeld_room_udn "$2") ;;
    --speaker-udn) opts+=(raumfeld_speaker_udn "$2") ;;
    --raumfeld-host) opts+=(raumfeld_host "$2") ;;
    --stream-format) opts+=(stream_format "$2") ;;
    --stream-host) opts+=(stream_host "$2") ;;
    --log-level) opts+=(log_level "$2") ;;
    --start-gate-seconds) opts+=(start_gate_seconds "$2") ;;
    --stuck-seconds) opts+=(stuck_seconds "$2") ;;
    --app-dir) app="$2" ;;
    *) usage; exit 2 ;;
  esac
  shift 2
done

[ -f "$app/artefacts/BUILD-INFO" ] || die "$app is not an assembled app folder; run scripts/build-ha-app.sh first"
for tool in mpd deno cargo python3 jq; do
  command -v "$tool" >/dev/null || die "'$tool' not found (brew install mpd deno jq; rustup for cargo)"
done
[ ! -e "$run" ] || die "$run exists: already running? stop it first with --stop"

# The receiver comes from this checkout; say so if it is not the app's.
receiver_commit="$(sed -n 's/^receiver: \([0-9a-f]*\).*/\1/p' "$app/artefacts/BUILD-INFO")"
head_commit="$(git -C "$root" rev-parse HEAD)"
if [ "$receiver_commit" != "$head_commit" ] || [ -n "$(git -C "$root" status --porcelain -- src Cargo.toml Cargo.lock build.rs proto)" ]; then
  echo "warning: the receiver is built from this checkout ($head_commit, plus any local changes), the app's from $receiver_commit" >&2
fi
echo "==> building the receiver for macOS (cargo build --release --locked)"
(cd "$root" && cargo build --release --locked --quiet)

mkdir -m 0700 "$run"
mkdir -m 0700 "$run/data" "$run/data/ytcast" "$run/data/mpd" "$run/data/mpd/music" \
  "$run/data/mpd/playlists" "$run/data/adapter"
# If anything below fails, stop what has started.
on_exit() {
  local status=$?
  [ "$status" = 0 ] || stop
  exit "$status"
}
trap on_exit EXIT

python3 -I - "${opts[@]}" > "$run/data/options.json" <<'PY'
import json, sys
args = sys.argv[1:]
options = dict(zip(args[0::2], args[1::2]))
for name in ("start_gate_seconds", "stuck_seconds"):
    if name in options:
        try:
            options[name] = int(options[name])
        except ValueError:
            pass  # the app's check names the option
print(json.dumps(options, indent=2))
PY
rootfs="$app/rootfs"
python3 -I "$rootfs/usr/lib/ytcast/check_options.py" "$run/data/options.json" \
  || die "invalid options (see above)"

option() { jq -r --arg k "$1" '.[$k] // empty' "$run/data/options.json"; }

# The app's own encoder choice and MPD configuration, with /data moved here.
# shellcheck disable=SC2329  # called by common.sh
bashio::exit.nok() { die "$*"; }
# shellcheck source=SCRIPTDIR/../ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh
source "$rootfs/usr/lib/ytcast/common.sh"
stream_format="$(option stream_format)"
encoder="$(ytcast_stream_encoder "$stream_format")"
template="$(< "$rootfs/etc/ytcast/mpd.conf.template")"
printf '%s\n' "${template/@STREAM_ENCODER@/${encoder}}" | sed "s|\"/data/|\"$run/data/|" > "$run/mpd.conf"

# Starts a process in the background with only the given environment; its
# output goes to $run/<name>.log.
start() {
  local name="$1"; shift
  env -i PATH=/usr/bin:/bin "$@" > "$run/$name.log" 2>&1 &
  echo "$name $!" >> "$run/pids"
  echo "started $name ($!), log $run/$name.log"
}

start mpd HOME="$run" "$(command -v mpd)" --no-daemon --stderr "$run/mpd.conf"
tries=40
until (exec 3<>/dev/tcp/127.0.0.1/6600) 2>/dev/null; do
  tries=$((tries - 1))
  [ "$tries" -gt 0 ] || { cat "$run/mpd.log"; die "MPD does not answer on 127.0.0.1:6600"; }
  sleep 0.25
done

# The helper's Deno cache was filled at build time for the app folder's copy.
bgutil="$app/artefacts/bgutil"
start potoken DENO_DIR="$bgutil/.deno" DENO_NO_UPDATE_CHECK=1 NO_COLOR=1 HOME=/nonexistent \
  "$(command -v deno)" run --no-prompt --frozen --cached-only \
  --allow-net=127.0.0.1:4416,www.youtube.com:443,www.google.com:443,jnn-pa.googleapis.com:443 \
  --allow-read="$bgutil" --allow-env \
  --deny-write --deny-run --deny-ffi --deny-sys --deny-import \
  "$bgutil/src/main.ts" --host 127.0.0.1 --port 4416

start receiver HOME="$run/data/ytcast" DEVICE_NAME="$(option cast_name)" \
  RUST_LOG="$(case "$(option log_level)" in debug) echo "info,ytcast_lite=debug" ;; warning) echo warn ;; *) option log_level ;; esac)" \
  MPD_HOST=127.0.0.1 MPD_PORT=6600 YTCAST_DATA_DIR="$run/data/ytcast" \
  "$root/target/release/ytcast-lite"

stream_host="$(option stream_host)"
if [ -z "$stream_host" ]; then
  stream_host="$(python3 -I -c 'import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(("192.0.2.1", 9))
print(s.getsockname()[0])')"
fi
adapter_env=(
  RAUMFELD_ROOM_UDN="$(option raumfeld_room_udn)"
  STREAM_URL="http://$stream_host:8000/stream.$stream_format"
  STREAM_MIME="$(ytcast_stream_mime "$stream_format")"
  MPD_HOST=127.0.0.1 MPD_PORT=6600
  LOG_LEVEL="$(option log_level | tr '[:lower:]' '[:upper:]')"
  ADAPTER_STATE_DIR="$run/data/adapter" HOME=/nonexistent
)
for pair in raumfeld_speaker_udn:RAUMFELD_SPEAKER_UDN raumfeld_host:RAUMFELD_HOST \
            start_gate_seconds:START_GATE_SECONDS stuck_seconds:STUCK_SECONDS; do
  value="$(option "${pair%%:*}")"
  [ -z "$value" ] || adapter_env+=("${pair#*:}=$value")
done
start adapter "${adapter_env[@]}" "$(command -v python3)" -I "$app/artefacts/adapter/raumfeld_adapter.py"

echo
echo "running: '$(option cast_name)', $stream_format stream at http://$stream_host:8000/stream.$stream_format"
echo "logs:    tail -f $run/*.log"
echo "stop:    scripts/run-ha-app-local.sh --stop"
