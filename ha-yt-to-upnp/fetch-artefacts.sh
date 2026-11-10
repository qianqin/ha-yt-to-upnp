#!/bin/sh
# Runs in the Dockerfile's artefacts stage: puts the prebuilt programs into OUT.
# STAGE holds config.yaml, artefacts.lock and, if the build context has an artefacts/ folder
# (assembled by scripts/build-ha-app.sh), that folder's contents; those are
# used as they are. Otherwise (the app folder as Home Assistant takes it from
# the repository) the release tarball named in artefacts.lock is downloaded over
# https and used only if its sha256 matches the lock.
# usage: fetch-artefacts.sh STAGE OUT
set -eu

[ $# -eq 2 ] || { echo "usage: fetch-artefacts.sh STAGE OUT" >&2; exit 2; }
stage="$1"
out="$2"
readonly ENTRIES="BUILD-INFO ytcast-lite deno adapter bgutil"

die() { echo "fetch-artefacts: error: $*" >&2; exit 1; }
lock_value() { sed -n "s/^$1: //p" "$stage/artefacts.lock"; }
# No setuid or setgid bit survives, whichever way the programs came.
strip_special_bits() { chmod -R u-s,g-s "$out"; }

mkdir -p "$out"
if [ -f "$stage/BUILD-INFO" ]; then
  echo "fetch-artefacts: artefacts/ from the build context"
  for entry in $ENTRIES; do
    [ -e "$stage/$entry" ] || die "artefacts/$entry missing from the build context"
    mv "$stage/$entry" "$out/"
  done
  strip_special_bits
  exit 0
fi

[ -f "$stage/artefacts.lock" ] || die "no artefacts/ folder and no artefacts.lock in the build context"
version="$(lock_value version)"
url="$(lock_value url)"
sha256="$(lock_value sha256)"
echo "$sha256" | grep -Eqx '[0-9a-f]{64}' || die "artefacts.lock has no valid sha256"
[ -n "$version" ] && [ -n "$url" ] || die "artefacts.lock has no version or url"
# A lock left from an older release would install old programs under the new
# version. config.yaml's version decides; the Supervisor's BUILD_VERSION, when
# it passes one, must agree with it.
app_version="$(sed -n 's/^version: "\(.*\)"$/\1/p' "$stage/config.yaml")"
[ -n "$app_version" ] || die "no version in config.yaml"
[ "$version" = "$app_version" ] \
  || die "artefacts.lock is for version $version, config.yaml says $app_version (run scripts/build-ha-app.sh --release)"
if [ -n "${BUILD_VERSION:-}" ] && [ "$BUILD_VERSION" != "$app_version" ]; then
  die "BUILD_VERSION $BUILD_VERSION is not config.yaml's version $app_version"
fi
proto='=https'
# Set only by scripts/smoke-ha-app.sh --release-path, which serves the tarball
# over plain http on the build machine; the sha256 still comes from the lock.
# Plain http only to this machine; a build.yaml must never set it.
if [ -n "${ARTEFACTS_URL:-}" ]; then
  # Strict: no user info, so "http://127.0.0.1:1@elsewhere/" cannot pass.
  if echo "$ARTEFACTS_URL" | grep -Eqx 'http://(host\.docker\.internal|127\.0\.0\.1):[0-9]{1,5}/[A-Za-z0-9._/-]*'; then
    proto='=http'
  elif ! echo "$ARTEFACTS_URL" | grep -Eqx 'https://[^@[:space:]]+'; then
    die "ARTEFACTS_URL must be https:// or http://host.docker.internal:<port>/ or http://127.0.0.1:<port>/"
  fi
  url="$ARTEFACTS_URL"
fi

echo "fetch-artefacts: $url"
tarball="$(mktemp)"
curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 --retry 3 --max-filesize 512M \
    -o "$tarball" "$url" \
  || die "download of $url failed"
actual="$(sha256sum "$tarball" | cut -d' ' -f1)"
[ "$actual" = "$sha256" ] || die "sha256 mismatch for $url: expected $sha256, got $actual"
echo "fetch-artefacts: sha256 $actual matches artefacts.lock"

unpack="$(mktemp -d)"
tar -xzf "$tarball" --no-same-owner --no-same-permissions -C "$unpack"
for entry in $ENTRIES; do
  [ -e "$unpack/artefacts/$entry" ] || die "artefacts/$entry missing from the release tarball"
  mv "$unpack/artefacts/$entry" "$out/"
done
rm -rf "$tarball" "$unpack"
strip_special_bits
