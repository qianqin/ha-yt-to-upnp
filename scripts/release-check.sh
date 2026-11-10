#!/usr/bin/env bash
# Runs every local release check on the current commit, in order, and stops at
# the first failure. Push to master only if this script exits 0.
# Docker resources created here are always removed on exit; images that were
# already present before the run (possibly used by others) are left alone.
set -eu

usage() {
  echo "usage: scripts/release-check.sh [--with-ubuntu]" >&2
  echo "  --with-ubuntu  also build and test in ubuntu:24.04 with a rustup-installed toolchain" >&2
}

with_ubuntu=0
for arg in "$@"; do
  case "$arg" in
    --with-ubuntu) with_ubuntu=1 ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is not clean; the checks must run on the exact commit you push" >&2
  exit 1
fi

# Home Assistant would build the app from tracked artefacts instead of the release.
if [ -n "$(git ls-files ha-yt-to-upnp/artefacts)" ]; then
  echo "error: ha-yt-to-upnp/artefacts is tracked; remove it from git" >&2
  exit 1
fi

for tool in cargo protoc docker python3; do
  command -v "$tool" >/dev/null || { echo "error: '$tool' not found (see docs/DEVOPS.md, Prerequisites)" >&2; exit 1; }
done
cargo audit --version >/dev/null 2>&1 || { echo "error: cargo-audit not installed (see docs/DEVOPS.md, Prerequisites)" >&2; exit 1; }

channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
[ -n "$channel" ] || { echo "error: no channel in rust-toolchain.toml" >&2; exit 1; }
rust_image="rust:${channel}-bookworm"
ubuntu_image="ubuntu:24.04"

run_id="ytcast-release-$$"
containers=""
volumes=""
pulled_images=""

cleanup() {
  status=$?
  set +e
  for c in $containers; do docker rm -f "$c" >/dev/null 2>&1; done
  for v in $volumes; do docker volume rm "$v" >/dev/null 2>&1; done
  for i in $pulled_images; do docker image rm "$i" >/dev/null 2>&1; done
  echo "==> cleanup: removed volumes [${volumes# }] images [${pulled_images# }]"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

step() { echo; echo "==> $*"; }

# Pulls the image unless it already exists; only images pulled here are removed on exit.
ensure_image() {
  if ! docker image inspect "$1" >/dev/null 2>&1; then
    pulled_images="$pulled_images $1"
    docker pull -q "$1" >/dev/null
  fi
}

# Source is mounted read-only; build output goes to a throwaway named volume.
linux_check() {
  name="$run_id-$1"; image="$2"; script="$3"
  ensure_image "$image"
  volumes="$volumes $name"
  docker volume create "$name" >/dev/null
  containers="$containers $name"
  docker run --rm --name "$name" \
    -v "$root":/src:ro -w /src \
    -v "$name":/target -e CARGO_TARGET_DIR=/target \
    "$image" bash -c "$script"
}

step "commit $(git rev-parse HEAD)"

step "1/4 macOS: cargo build --locked && cargo test --locked"
cargo build --locked
cargo test --locked

step "2/4 Linux ($rust_image): cargo build --locked && cargo test --locked"
linux_check debian "$rust_image" \
  'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo build --locked && cargo test --locked'

if [ "$with_ubuntu" = 1 ]; then
  step "2b/4 Linux ($ubuntu_image + rustup): cargo build --locked && cargo test --locked"
  linux_check ubuntu "$ubuntu_image" \
    "apt-get update -qq && apt-get install -y -qq curl ca-certificates build-essential pkg-config protobuf-compiler >/dev/null \
     && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none \
     && . \"\$HOME/.cargo/env\" && cargo build --locked && cargo test --locked"
fi

step "3/4 cargo audit"
cargo audit

step "4/4 adapter: python3 -m unittest discover -s adapter"
python3 -m unittest discover -s adapter

step "all checks passed on $(git rev-parse HEAD)"
