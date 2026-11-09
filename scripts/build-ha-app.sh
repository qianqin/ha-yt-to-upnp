#!/usr/bin/env bash
# Assembles the Home Assistant app folder, ready to copy into the HA host's
# /addons share: ha-yt-to-upnp/ as committed at HEAD plus prebuilt artefacts for
# linux/amd64, so the Supervisor's build only installs Debian packages and
# copies files. The build images are pinned by digest, Deno by checksum,
# bgutil by commit and lock files; any mismatch aborts. Not pinned: protoc
# (Debian bookworm's protobuf-compiler, installed in the Rust container); its
# version is recorded in BUILD-INFO.
# Temporary directories, containers and volumes created here are always
# removed, and so are images this script had to pull (images that were already
# present are left alone).
set -euo pipefail

# Pinned inputs. Update deliberately, re-verify, and note it in the CHANGELOG.
readonly PLATFORM=linux/amd64
readonly RUST_TARGET=x86_64-unknown-linux-gnu
# Must equal the channel in the receiver's rust-toolchain.toml (checked below),
# so rustup in the container downloads nothing.
readonly RUST_CHANNEL=1.99.1
readonly RUST_IMAGE="rust:${RUST_CHANNEL}-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c"
readonly NODE_IMAGE="node:26.11.1-bookworm-slim@sha256:86f07bc9c5dce4578cf37e5a418b7bfc7f817cda25cde66e2b66e95ed86c4567"
readonly DENO_VERSION=2.9.8
readonly DENO_URL="https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-${RUST_TARGET}.zip"
readonly DENO_SHA256=e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609
readonly BGUTIL_REPO=https://github.com/Brainicism/bgutil-ytdlp-pot-provider.git
readonly BGUTIL_COMMIT=26475e9d3665b972c4aeb83b4dbaafcf64b88438  # tag 2.0.2
# The helper's path inside the image: Deno's cache is filled for exactly this path.
readonly BGUTIL_DIR=/opt/bgutil

usage() {
  cat >&2 <<'EOF'
usage: scripts/build-ha-app.sh [--receiver-ref REF] [--adapter-ref REF] [--out DIR]
  --receiver-ref REF  commit of the receiver (Rust) to build (default 7a699f5)
  --adapter-ref REF   commit to take adapter/raumfeld_adapter.py from (default 556d421)
  --out DIR           output folder (default dist/ha-yt-to-upnp); replaced only if it
                      holds a previous build (artefacts/BUILD-INFO) or is empty
EOF
}

receiver_ref=7a699f5
adapter_ref=556d421
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --receiver-ref) [ $# -ge 2 ] || { usage; exit 2; }; receiver_ref="$2"; shift 2 ;;
    --adapter-ref) [ $# -ge 2 ] || { usage; exit 2; }; adapter_ref="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || { usage; exit 2; }; out="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done

root="$(git rev-parse --show-toplevel)"
cd "$root"
out="${out:-$root/dist/ha-yt-to-upnp}"

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

for tool in docker git curl unzip shasum tar file; do
  command -v "$tool" >/dev/null || die "'$tool' not found"
done
docker info >/dev/null 2>&1 || die "Docker daemon not running"

# The app folder is taken from HEAD; uncommitted changes there would be silently
# left out of the build, so refuse them.
if [ -n "$(git status --porcelain --untracked-files=all -- ha-yt-to-upnp)" ]; then
  git status --short --untracked-files=all -- ha-yt-to-upnp >&2
  die "uncommitted or untracked changes in ha-yt-to-upnp/; commit them first"
fi
head_commit="$(git rev-parse --verify HEAD)"

if [ -e "$out" ] && [ ! -f "$out/artefacts/BUILD-INFO" ] && [ -n "$(ls -A "$out")" ]; then
  die "$out exists and is not a previous build; refusing to replace it"
fi

receiver_commit="$(git rev-parse --verify --quiet "${receiver_ref}^{commit}")" || die "unknown receiver ref '$receiver_ref'"
adapter_commit="$(git rev-parse --verify --quiet "${adapter_ref}^{commit}")" || die "unknown adapter ref '$adapter_ref'"
channel="$(git show "$receiver_commit:rust-toolchain.toml" | sed -n 's/^channel = "\(.*\)"$/\1/p')"
[ -n "$channel" ] || die "no channel in rust-toolchain.toml at $receiver_commit"
[ "$channel" = "$RUST_CHANNEL" ] || die "receiver $receiver_commit needs Rust $channel, the pinned image has $RUST_CHANNEL; update RUST_CHANNEL and RUST_IMAGE"

run_id="ytcast-ha-build-$$"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ytcast-ha-build.XXXXXX")"
containers=""
volumes=""
pulled_images=""

cleanup() {
  status=$?
  set +e
  for c in $containers; do docker rm -f "$c" >/dev/null 2>&1; done
  for v in $volumes; do docker volume rm "$v" >/dev/null 2>&1; done
  for i in $pulled_images; do docker image rm "$i" >/dev/null 2>&1; done
  # Files written by root inside the build containers are owned by the user on
  # Docker Desktop; elsewhere they may need the container to remove them.
  rm -rf "$tmp" 2>/dev/null || echo "warning: could not remove $tmp completely" >&2
  echo "==> cleanup: removed temp dir, volumes [${volumes# }], images [${pulled_images# }]"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# Pulls the image unless it already exists; only images pulled here are removed on exit.
ensure_image() {
  if ! docker image inspect "$1" >/dev/null 2>&1; then
    pulled_images="$pulled_images $1"
    docker pull -q --platform "$PLATFORM" "$1" >/dev/null
  fi
}

sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }

app="$tmp/app"
art="$app/artefacts"
mkdir -p "$art/adapter" "$tmp/dl" "$tmp/deno" "$tmp/receiver-src" "$tmp/receiver-out"

step "receiver $receiver_commit ($RUST_IMAGE, $PLATFORM, --release --locked)"
git archive "$receiver_commit" | tar -x -C "$tmp/receiver-src"
ensure_image "$RUST_IMAGE"
volumes="$volumes $run_id-target"
docker volume create "$run_id-target" >/dev/null
containers="$containers $run_id-receiver"
docker run --rm --name "$run_id-receiver" --platform "$PLATFORM" \
  -v "$tmp/receiver-src":/src:ro -w /src \
  -v "$run_id-target":/target -e CARGO_TARGET_DIR=/target \
  -v "$tmp/receiver-out":/out \
  "$RUST_IMAGE" bash -c "apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null \
    && protoc --version > /out/protoc-version \
    && cargo build --locked --release --target $RUST_TARGET \
    && install -m 0755 /target/$RUST_TARGET/release/ytcast-lite /out/ytcast-lite"
file_type="$(file -b "$tmp/receiver-out/ytcast-lite")"
case "$file_type" in
  *ELF\ 64-bit*x86-64*) ;;
  *) die "receiver binary is not an x86-64 ELF: $file_type" ;;
esac
cp "$tmp/receiver-out/ytcast-lite" "$art/ytcast-lite"
protoc_version="$(cat "$tmp/receiver-out/protoc-version")"

step "adapter $adapter_commit"
git show "$adapter_commit:adapter/raumfeld_adapter.py" > "$art/adapter/raumfeld_adapter.py"

step "Deno $DENO_VERSION ($DENO_URL)"
curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/dl/deno.zip" "$DENO_URL"
actual="$(sha256_of "$tmp/dl/deno.zip")"
[ "$actual" = "$DENO_SHA256" ] || die "Deno checksum mismatch: expected $DENO_SHA256, got $actual"
unzip -q "$tmp/dl/deno.zip" -d "$tmp/deno"
[ -f "$tmp/deno/deno" ] || die "deno binary missing from the release zip"
install -m 0755 "$tmp/deno/deno" "$art/deno"

step "bgutil-ytdlp-pot-provider $BGUTIL_COMMIT"
git init -q "$tmp/bgutil-src"
git -C "$tmp/bgutil-src" fetch -q --depth 1 "$BGUTIL_REPO" "$BGUTIL_COMMIT"
git -C "$tmp/bgutil-src" checkout -q FETCH_HEAD
[ "$(git -C "$tmp/bgutil-src" rev-parse HEAD)" = "$BGUTIL_COMMIT" ] || die "bgutil checkout is not $BGUTIL_COMMIT"
mkdir -p "$art/bgutil"
(cd "$tmp/bgutil-src/server" && cp -R package.json package-lock.json deno.json deno.lock tsconfig.json src types "$art/bgutil/")
cp "$tmp/bgutil-src/LICENSE" "$art/bgutil/LICENSE"

step "bgutil: npm ci --omit=dev --ignore-scripts + Deno cache ($NODE_IMAGE, $PLATFORM)"
# npm ci checks every package against package-lock.json's integrity hashes and
# deno cache --frozen against deno.lock, so both fail closed. No install
# scripts run. The cache is filled at the path the image uses.
ensure_image "$NODE_IMAGE"
containers="$containers $run_id-bgutil"
docker run --rm --name "$run_id-bgutil" --platform "$PLATFORM" \
  -v "$art/bgutil":"$BGUTIL_DIR" -w "$BGUTIL_DIR" \
  -v "$art/deno":/usr/local/bin/deno:ro \
  -e DENO_DIR="$BGUTIL_DIR/.deno" -e DENO_NO_UPDATE_CHECK=1 -e NO_COLOR=1 \
  "$NODE_IMAGE" bash -c "npm ci --omit=dev --ignore-scripts --no-audit --no-fund --loglevel=warn \
    && deno cache --frozen src/main.ts"
[ -d "$art/bgutil/node_modules" ] && [ -d "$art/bgutil/.deno" ] || die "bgutil install incomplete"
# deno.json allows canvas' install script, but neither npm ci --ignore-scripts
# nor deno cache runs it; a native addon here would mean that something did.
native="$(find "$art/bgutil" \( -name '*.node' -o -path '*/node_modules/canvas/build' \) -print)"
[ -z "$native" ] || die "bgutil contains native build output (an install script ran): $native"

step "app folder (ha-yt-to-upnp/ at $head_commit)"
git archive "$head_commit" ha-yt-to-upnp | tar -x -C "$tmp"
cp -R "$tmp/ha-yt-to-upnp/." "$app/"

cat > "$art/BUILD-INFO" <<EOF
built: $(date -u +%Y-%m-%dT%H:%M:%SZ)
app folder: $head_commit (ha-yt-to-upnp/ via git archive)
receiver: $receiver_commit ($RUST_TARGET, release, --locked)
receiver build image: $RUST_IMAGE
receiver protoc: $protoc_version (Debian bookworm protobuf-compiler)
receiver sha256: $(sha256_of "$art/ytcast-lite")
adapter: $adapter_commit (adapter/raumfeld_adapter.py)
adapter sha256: $(sha256_of "$art/adapter/raumfeld_adapter.py")
deno: $DENO_VERSION, zip sha256 $DENO_SHA256
deno binary sha256: $(sha256_of "$art/deno")
bgutil-ytdlp-pot-provider: $BGUTIL_COMMIT (npm ci --omit=dev --ignore-scripts in $NODE_IMAGE)
bgutil native build output: none (no *.node, no node_modules/canvas/build)
EOF

step "output $out"
rm -rf "$out"
mkdir -p "$(dirname "$out")"
mv "$app" "$out"
cat "$out/artefacts/BUILD-INFO"
echo "==> done: copy $out to the HA host's /addons share (see docs/DEVOPS.md)"
