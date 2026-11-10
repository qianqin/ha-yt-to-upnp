#!/bin/sh
# Runs in the Dockerfile's artefacts stage: puts the prebuilt programs into OUT.
# STAGE holds artefacts.lock and, if the build context has an artefacts/ folder
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

mkdir -p "$out"
if [ -f "$stage/BUILD-INFO" ]; then
  echo "fetch-artefacts: artefacts/ from the build context"
  for entry in $ENTRIES; do
    [ -e "$stage/$entry" ] || die "artefacts/$entry missing from the build context"
    mv "$stage/$entry" "$out/"
  done
  exit 0
fi

[ -f "$stage/artefacts.lock" ] || die "no artefacts/ folder and no artefacts.lock in the build context"
version="$(lock_value version)"
url="$(lock_value url)"
sha256="$(lock_value sha256)"
echo "$sha256" | grep -Eqx '[0-9a-f]{64}' || die "artefacts.lock has no valid sha256"
[ -n "$version" ] && [ -n "$url" ] || die "artefacts.lock has no version or url"
# The Supervisor passes the app's version: a lock left from an older release
# would install old programs under the new version.
if [ -n "${BUILD_VERSION:-}" ] && [ "$BUILD_VERSION" != "$version" ]; then
  die "artefacts.lock is for version $version, the app is $BUILD_VERSION (run scripts/build-ha-app.sh --release)"
fi
proto='=https'
# Set only by scripts/smoke-ha-app.sh --release-path, which serves the tarball
# over plain http on the build machine; the sha256 still comes from the lock.
if [ -n "${ARTEFACTS_URL:-}" ]; then
  url="$ARTEFACTS_URL"
  proto='=http,https'
fi

echo "fetch-artefacts: $url"
tarball="$(mktemp)"
curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 --retry 3 -o "$tarball" "$url" \
  || die "download of $url failed"
actual="$(sha256sum "$tarball" | cut -d' ' -f1)"
[ "$actual" = "$sha256" ] || die "sha256 mismatch for $url: expected $sha256, got $actual"
echo "fetch-artefacts: sha256 $actual matches artefacts.lock"

unpack="$(mktemp -d)"
tar -xzf "$tarball" --no-same-owner -C "$unpack"
for entry in $ENTRIES; do
  [ -e "$unpack/artefacts/$entry" ] || die "artefacts/$entry missing from the release tarball"
  mv "$unpack/artefacts/$entry" "$out/"
done
rm -rf "$tarball" "$unpack"
