#!/usr/bin/env bash
# Publishes the release tarball of the version at HEAD as a GitHub release
# asset, for Home Assistant's build from the repository (see docs/DEVOPS.md,
# Release). Run it after scripts/build-ha-app.sh --release, the commit of
# ha-yt-to-upnp/artefacts.lock and the smoke tests, and before pushing master.
#
# Never replaces anything: it stops if the tag v<version> exists locally or on
# GitHub, or if a release of that name exists. A published version is fixed;
# a fix is a new version.
#
# Steps: push the tag v<version> (HEAD) to origin; create a draft release from
# it with the tarball and the version's CHANGELOG section; download the draft's
# asset and check its sha256; publish the draft; download the asset from its
# public URL (the one in artefacts.lock) and check the sha256 again. It does not
# push master: that is the last step, by hand, once this script has passed.
#
# --dry-run: the local checks only (no network), then the commands it would run.
set -euo pipefail

usage() { echo "usage: scripts/publish-release.sh [--dry-run]" >&2; }

dry_run=0
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  -h|--help) usage; exit 0 ;;
  *) usage; exit 2 ;;
esac
[ $# -le 1 ] || { usage; exit 2; }

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

root="$(git rev-parse --show-toplevel)"
cd "$root"

for tool in git gh curl shasum python3; do
  command -v "$tool" >/dev/null || die "'$tool' not found"
done
[ -z "$(git status --porcelain)" ] || die "working tree is not clean; publish from the exact commit you push"

head_commit="$(git rev-parse --verify HEAD)"
version="$(git show "HEAD:ha-yt-to-upnp/config.yaml" | sed -n 's/^version: "\(.*\)"$/\1/p')"
[ -n "$version" ] || die "no version in ha-yt-to-upnp/config.yaml"
tag="v$version"
lock="$(git show "HEAD:ha-yt-to-upnp/artefacts.lock" 2>/dev/null)" || die "no ha-yt-to-upnp/artefacts.lock at HEAD; run scripts/build-ha-app.sh --release and commit it"
lock_value() { sed -n "s/^$1: //p" <<<"$lock"; }
[ "$(lock_value version)" = "$version" ] || die "artefacts.lock is for version $(lock_value version), config.yaml says $version"

# The URL that Home Assistant downloads must be the asset this script uploads.
read -r repo expected_url asset < <(python3 -I -B -c 'import sys; sys.path.insert(0, sys.argv[1]); import pack_artefacts as p
r = p.github_repo(sys.argv[2]); print("%s/%s" % r, p.asset_url(r, sys.argv[3]), p.asset_name(sys.argv[3]))' \
  "$root/scripts" "$(git remote get-url origin)" "$version")
url="$(lock_value url)"
[ "$url" = "$expected_url" ] || die "artefacts.lock's url $url is not the release asset of origin ($expected_url)"
sha256="$(lock_value sha256)"
tarball="$root/dist/release/$asset"
[ -f "$tarball" ] || die "no $tarball; run scripts/build-ha-app.sh --release"
actual="$(shasum -a 256 "$tarball" | cut -d' ' -f1)"
[ "$actual" = "$sha256" ] || die "$tarball has sha256 $actual, artefacts.lock says $sha256"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/ytcast-publish.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
notes="$tmp/notes.md"
awk -v heading="## $version" '$0 == heading { on = 1; next } on && /^## / { exit } on { print }' \
  ha-yt-to-upnp/CHANGELOG.md > "$notes"
grep -q '[^[:space:]]' "$notes" || die "no '## $version' section in ha-yt-to-upnp/CHANGELOG.md"

echo "version $version, commit $head_commit, repository $repo"
echo "asset $tarball"
echo "sha256 $sha256"
echo "url $url"

if [ "$dry_run" = 1 ]; then
  step "dry run: local checks passed; the commands would be"
  cat <<EOF
git push origin $head_commit:refs/tags/$tag
gh release create $tag $tarball --repo $repo --verify-tag --draft --title $version --notes-file <CHANGELOG section $version>
gh release download $tag --repo $repo --pattern $asset --dir <tmp>   # sha256 must be $sha256
gh release edit $tag --repo $repo --draft=false --latest
curl -fsSL --proto '=https' -o <tmp> $url   # sha256 must be $sha256
EOF
  exit 0
fi

check_download() {
  local file="$1" got
  got="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  [ "$got" = "$sha256" ] || die "$2 has sha256 $got, expected $sha256"
  echo "sha256 of $2 matches"
}

step "nothing named $tag exists yet"
gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login)"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then die "local tag $tag exists; a published version is fixed, bump the version"; fi
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then die "tag $tag exists on origin; bump the version"; fi
if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then die "release $tag exists; bump the version"; fi

step "push tag $tag ($head_commit)"
# Without --force: refused if the tag appeared in the meantime.
git push origin "$head_commit:refs/tags/$tag"

step "draft release $tag with $asset"
gh release create "$tag" "$tarball" --repo "$repo" --verify-tag --draft --title "$version" --notes-file "$notes"
gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$tmp/draft"
check_download "$tmp/draft/$asset" "the draft's asset"

step "publish release $tag"
gh release edit "$tag" --repo "$repo" --draft=false --latest
curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 -o "$tmp/public" "$url"
check_download "$tmp/public" "$url"

step "published $tag; now push master: git push origin $head_commit:refs/heads/master"
