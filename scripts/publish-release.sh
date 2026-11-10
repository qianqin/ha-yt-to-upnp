#!/usr/bin/env bash
# Publishes the release tarball of the version at HEAD as a GitHub release
# asset, for Home Assistant's build from the repository (see docs/DEVOPS.md,
# App release). Run it on master after scripts/build-ha-app.sh --release, the
# commit of ha-yt-to-upnp/artefacts.lock and the smoke tests, and before
# pushing master.
#
# Never replaces anything: it stops if the tag v<version> exists locally or on
# GitHub, or if a release of that name exists. A published version is fixed;
# a fix is a new version.
#
# Before anything leaves this machine it checks that the tarball belongs to
# HEAD: the lock at HEAD is for config.yaml's version, names the asset URL of
# the origin repository and has the tarball's sha256; and the receiver
# sources, the adapter and the app folder (but artefacts.lock and *.md) are the
# same at HEAD as at the commits that the tarball's BUILD-INFO names.
#
# Steps: fetch origin's master (HEAD must be on top of it, on branch master);
# push the tag v<version> (HEAD) to origin; create a draft release from it with
# the tarball and the version's CHANGELOG section; download the draft's asset
# and check its sha256; publish the draft; download the asset from its public
# URL (the one in artefacts.lock, retried while GitHub does not serve it yet)
# and check the sha256 again. It does not push master: that is the last step,
# by hand, once this script has passed.
#
# --dry-run: the local checks only (no network; the branch is shown, not
# required), then the commands it would run.
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

# Receiver sources and the app folder without what may change after the build.
readonly RECEIVER_PATHS=(src Cargo.toml Cargo.lock rust-toolchain.toml build.rs proto)
readonly ADAPTER_PATHS=(adapter/raumfeld_adapter.py)
readonly APP_PATHS=(ha-yt-to-upnp ':(exclude)ha-yt-to-upnp/artefacts.lock' ':(exclude,glob)ha-yt-to-upnp/**/*.md')
readonly PUBLIC_TRIES=6 PUBLIC_WAIT_S=10

root="$(git rev-parse --show-toplevel)"
cd "$root"

for tool in git gh curl shasum python3 tar; do
  command -v "$tool" >/dev/null || die "'$tool' not found"
done
[ -z "$(git status --porcelain)" ] || die "working tree is not clean; publish from the exact commit you push"
[ -z "$(git ls-files ha-yt-to-upnp/artefacts)" ] \
  || die "ha-yt-to-upnp/artefacts is tracked: Home Assistant would build from those files, not the release"

head_commit="$(git rev-parse --verify HEAD)"
branch="$(git symbolic-ref --short -q HEAD || echo "(detached)")"
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

# The tarball's programs must be the ones this commit's sources make.
build_info="$(tar -xzOf "$tarball" artefacts/BUILD-INFO)" || die "no artefacts/BUILD-INFO in $tarball"
built_commit() {
  local c
  c="$(sed -n "s/^$1: \([0-9a-f]\{40\}\) .*/\1/p" <<<"$build_info")"
  [ -n "$c" ] || die "BUILD-INFO names no $1 commit"
  git cat-file -e "$c^{commit}" 2>/dev/null || die "BUILD-INFO's $1 commit $c is not in this repository"
  echo "$c"
}
same_since() {
  local what="$1" commit="$2"; shift 2
  git diff --quiet "$commit" HEAD -- "$@" \
    || { git diff --stat "$commit" HEAD -- "$@" >&2; die "$what changed since the tarball's commit $commit; run scripts/build-ha-app.sh --release again"; }
  echo "BUILD-INFO: $what as at $commit"
}
# Plain assignments, so that set -e stops on built_commit's die.
receiver_commit="$(built_commit receiver)"
adapter_commit="$(built_commit adapter)"
app_commit="$(built_commit "app folder")"
same_since "receiver sources" "$receiver_commit" "${RECEIVER_PATHS[@]}"
same_since "adapter" "$adapter_commit" "${ADAPTER_PATHS[@]}"
same_since "app folder" "$app_commit" "${APP_PATHS[@]}"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/ytcast-publish.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
notes="$tmp/notes.md"
awk -v heading="## $version" '$0 == heading { on = 1; next } on && /^## / { exit } on { print }' \
  ha-yt-to-upnp/CHANGELOG.md > "$notes"
grep -q '[^[:space:]]' "$notes" || die "no '## $version' section in ha-yt-to-upnp/CHANGELOG.md"

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then die "local tag $tag exists; a published version is fixed, bump the version"; fi

echo "version $version, commit $head_commit, branch $branch, repository $repo"
echo "asset $tarball"
echo "sha256 $sha256"
echo "url $url"

if [ "$dry_run" = 1 ]; then
  [ "$branch" = master ] || echo "note: on branch $branch; the real run publishes from master only"
  step "dry run: local checks passed; the commands would be"
  cat <<EOF
git fetch -q origin master && git merge-base --is-ancestor origin/master HEAD
git ls-remote --exit-code --tags origin refs/tags/$tag   # must find nothing
gh release view $tag --repo $repo                          # must be "release not found"
git push origin $head_commit:refs/tags/$tag
gh release create $tag $tarball --repo $repo --verify-tag --draft --title $version --notes-file <CHANGELOG section $version>
gh release download $tag --repo $repo --pattern $asset --dir <tmp>   # sha256 must be $sha256
gh release edit $tag --repo $repo --draft=false --latest
curl -fsSL --proto '=https' -o <tmp> $url   # sha256 must be $sha256
EOF
  exit 0
fi

[ "$branch" = master ] || die "on branch $branch; publish from master (the commit that is pushed next)"

check_download() {
  local file="$1" got
  got="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  [ "$got" = "$sha256" ] || die "$2 has sha256 $got, expected $sha256"
  echo "sha256 of $2 matches"
}

step "HEAD is on top of origin's master; nothing named $tag exists yet"
gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login)"
git fetch -q origin master
git merge-base --is-ancestor origin/master HEAD \
  || die "origin/master is not an ancestor of HEAD; rebase onto it first"
if git merge-base --is-ancestor HEAD origin/master; then
  echo "warning: origin/master already has this commit, so Home Assistant offers $version before its release exists" >&2
fi
rc=0
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null || rc=$?
case "$rc" in
  2) ;;
  0) die "tag $tag exists on origin; bump the version" ;;
  *) die "could not list the tags on origin (git ls-remote exit $rc)" ;;
esac
rc=0
view="$(gh release view "$tag" --repo "$repo" 2>&1 >/dev/null)" || rc=$?
if [ "$rc" = 0 ]; then die "release $tag exists; bump the version"; fi
grep -qi "release not found" <<<"$view" || die "could not look up release $tag: $view"

step "push tag $tag ($head_commit)"
# Without --force: refused if the tag appeared in the meantime.
git push origin "$head_commit:refs/tags/$tag"

step "draft release $tag with $asset"
gh release create "$tag" "$tarball" --repo "$repo" --verify-tag --draft --title "$version" --notes-file "$notes"
gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$tmp/draft"
check_download "$tmp/draft/$asset" "the draft's asset"

step "publish release $tag"
gh release edit "$tag" --repo "$repo" --draft=false --latest
# GitHub may answer 404 for a moment after publishing.
for try in $(seq 1 "$PUBLIC_TRIES"); do
  if curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 -o "$tmp/public" "$url"; then
    break
  fi
  if [ "$try" = "$PUBLIC_TRIES" ]; then
    die "release $tag is published; only the check of its public download failed. Check it again: curl -fsSL -o <file> $url && shasum -a 256 <file> (must be $sha256)"
  fi
  sleep "$PUBLIC_WAIT_S"
done
check_download "$tmp/public" "$url"

step "published $tag; now push master: git push origin $head_commit:refs/heads/master"
