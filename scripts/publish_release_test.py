"""Tests of scripts/publish-release.sh with fake gh, curl and sleep, and a git
that passes everything through except the calls that reach origin.

Runs on the host, no network, no Docker: python3 -I scripts/publish_release_test.py

Each test builds a small repository (master, app folder, receiver and adapter
sources), a release tarball whose BUILD-INFO names its commit, and the
committed artefacts.lock, then runs the script and reads the fake tools' log.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.dont_write_bytecode = True
SCRIPTS = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, SCRIPTS)
import pack_artefacts  # noqa: E402

VERSION = "1.2.3"
TAG = "v" + VERSION
REMOTE = "https://github.com/owner/repo.git"
ASSET = pack_artefacts.asset_name(VERSION)
REAL_GIT = shutil.which("git")

FAKE_GIT = r"""#!/bin/bash
echo "git $*" >> "$FAKE_LOG"
case "$1" in
  push) exit "${FAKE_PUSH_RC:-0}" ;;
  ls-remote) exit "${FAKE_LS_REMOTE_RC:-2}" ;;
  fetch) exec "$REAL_GIT" update-ref refs/remotes/origin/master "${FAKE_ORIGIN_MASTER:-HEAD~1}" ;;
  remote) [ "$2" = get-url ] && { echo "$FAKE_REMOTE"; exit 0; } ;;
esac
exec "$REAL_GIT" "$@"
"""

# Writes the file named by FAKE_<WHAT>_ASSET where --dir/-o points.
FAKE_GH = r"""#!/bin/bash
echo "gh $*" >> "$FAKE_LOG"
case "$1 $2" in
  "auth status") exit 0 ;;
  "release view")
    [ "${FAKE_RELEASE:-absent}" = exists ] && exit 0
    [ "${FAKE_RELEASE:-absent}" = absent ] && { echo "release not found" >&2; exit 1; }
    echo "HTTP 502: Bad Gateway" >&2; exit 1 ;;
  "release create"|"release edit") exit 0 ;;
  "release download")
    dir="" pattern=""
    while [ $# -gt 0 ]; do
      case "$1" in --dir) dir="$2"; shift ;; --pattern) pattern="$2"; shift ;; esac
      shift
    done
    mkdir -p "$dir" && cp "$FAKE_DRAFT_ASSET" "$dir/$pattern" ;;
  *) echo "unexpected gh call" >&2; exit 1 ;;
esac
"""

FAKE_CURL = r"""#!/bin/bash
echo "curl $*" >> "$FAKE_LOG"
fails="$(cat "$FAKE_CURL_FAILS_FILE" 2>/dev/null || echo 0)"
if [ "$fails" -gt 0 ]; then echo $((fails - 1)) > "$FAKE_CURL_FAILS_FILE"; echo "curl: (22) 404" >&2; exit 22; fi
out=""
while [ $# -gt 0 ]; do [ "$1" = -o ] && { out="$2"; shift; }; shift; done
cp "$FAKE_PUBLIC_ASSET" "$out"
"""

FAKE_SLEEP = """#!/bin/bash
echo "sleep $*" >> "$FAKE_LOG"
"""


class PublishReleaseTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.realpath(self._tmp.name)
        self.repo = os.path.join(self.tmp, "repo")
        self.log = os.path.join(self.tmp, "calls.log")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        for name, body in (("git", FAKE_GIT), ("gh", FAKE_GH), ("curl", FAKE_CURL), ("sleep", FAKE_SLEEP)):
            path = os.path.join(self.bin, name)
            with open(path, "w") as f:
                f.write(body)
            os.chmod(path, 0o755)
        self.env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"], REAL_GIT=REAL_GIT,
                        FAKE_LOG=self.log, FAKE_REMOTE=REMOTE,
                        FAKE_CURL_FAILS_FILE=os.path.join(self.tmp, "curl-fails"),
                        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
                        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid",
                        GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.make_repo()

    def tearDown(self):
        self._tmp.cleanup()

    def git(self, *args):
        return subprocess.run([REAL_GIT, *args], cwd=self.repo, env=self.env, check=True,
                              capture_output=True, text=True).stdout.strip()

    def write(self, rel, text):
        path = os.path.join(self.repo, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def make_repo(self):
        os.makedirs(self.repo)
        self.git("init", "-q", "-b", "master")
        self.write(".gitignore", "/dist\n")
        for rel in ("src/main.rs", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "build.rs",
                    "proto/a.proto", "adapter/raumfeld_adapter.py", "ha-yt-to-upnp/rootfs/run"):
            self.write(rel, rel + "\n")
        self.write("ha-yt-to-upnp/config.yaml", 'name: x\nversion: "%s"\n' % VERSION)
        self.write("ha-yt-to-upnp/CHANGELOG.md", "# Changelog\n\n## %s\n\n- New.\n\n## 1.2.2\n\n- Old.\n" % VERSION)
        os.makedirs(os.path.join(self.repo, "scripts"))
        for name in ("publish-release.sh", "pack_artefacts.py"):
            shutil.copy2(os.path.join(SCRIPTS, name), os.path.join(self.repo, "scripts", name))
        self.first = self.commit("first")
        self.built = self.commit_release(self.first)

    def commit_release(self, built_from):
        """Packs a tarball whose BUILD-INFO names built_from and commits its lock."""
        art = os.path.join(self.tmp, "artefacts-" + built_from[:8])
        os.makedirs(os.path.join(art, "adapter"))
        with open(os.path.join(art, "BUILD-INFO"), "w") as f:
            f.write(textwrap.dedent("""\
                built: now
                app folder: %(c)s (ha-yt-to-upnp/ via git archive)
                receiver: %(c)s (x86_64-unknown-linux-gnu, release, --locked)
                adapter: %(c)s (adapter/raumfeld_adapter.py)
                """) % {"c": built_from})
        with open(os.path.join(art, "ytcast-lite"), "w") as f:
            f.write("binary " + built_from)
        release = os.path.join(self.repo, "dist", "release")
        os.makedirs(release, exist_ok=True)
        self.tarball = os.path.join(release, ASSET)
        self.sha256 = pack_artefacts.pack(art, self.tarball, 1_700_000_000)
        pack_artefacts.write_lock(os.path.join(self.repo, "ha-yt-to-upnp", "artefacts.lock"), VERSION,
                                  pack_artefacts.asset_url(("owner", "repo"), VERSION), self.sha256)
        self.env["FAKE_DRAFT_ASSET"] = self.tarball
        self.env["FAKE_PUBLIC_ASSET"] = self.tarball
        return self.commit("lock")

    def run_script(self, *args, **env):
        if os.path.exists(self.log):
            os.remove(self.log)
        result = subprocess.run(["bash", "scripts/publish-release.sh", *args], cwd=self.repo,
                                env=dict(self.env, **env), capture_output=True, text=True)
        calls = []
        if os.path.exists(self.log):
            with open(self.log) as f:
                calls = f.read().splitlines()
        return result, calls

    def remote_calls(self, calls):
        """The calls that reach origin or GitHub, in order."""
        keep = ("git push", "git fetch", "git ls-remote", "gh ", "curl ")
        return [c for c in calls if c.startswith(keep)]

    def assertRefused(self, result, calls, message):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)
        self.assertFalse([c for c in calls if c.startswith(("git push", "gh release create"))], calls)

    # -- the release goes out in order --------------------------------------

    def test_publishes_tag_draft_check_publish_check_in_that_order(self):
        result, calls = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        steps = [c.split(" --")[0] for c in self.remote_calls(calls) if not c.startswith("gh auth")]
        self.assertEqual(steps, [
            "git fetch -q origin master",
            "git ls-remote",
            "gh release view %s" % TAG,
            "git push origin %s:refs/tags/%s" % (self.built, TAG),
            "gh release create %s %s" % (TAG, self.tarball),
            "gh release download %s" % TAG,
            "gh release edit %s" % TAG,
            "curl -fsSL",
        ])
        self.assertFalse([c for c in calls if c.startswith("git push") and "master" in c], calls)
        self.assertIn("now push master", result.stdout)

    def test_dry_run_reaches_nothing_remote(self):
        result, calls = self.run_script("--dry-run")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.remote_calls(calls), [])
        self.assertIn("BUILD-INFO", result.stdout)
        self.assertIn("git push origin %s:refs/tags/%s" % (self.built, TAG), result.stdout)

    # -- nothing named like the release may exist ----------------------------

    def test_existing_local_tag_is_refused_also_in_a_dry_run(self):
        self.git("tag", TAG)
        for args in ((), ("--dry-run",)):
            with self.subTest(args=args):
                result, calls = self.run_script(*args)
                self.assertRefused(result, calls, "local tag %s exists" % TAG)

    def test_existing_remote_tag_is_refused(self):
        result, calls = self.run_script(FAKE_LS_REMOTE_RC="0")
        self.assertRefused(result, calls, "tag %s exists on origin" % TAG)

    def test_failed_remote_tag_lookup_is_refused(self):
        result, calls = self.run_script(FAKE_LS_REMOTE_RC="128")
        self.assertRefused(result, calls, "could not list the tags on origin")

    def test_existing_release_is_refused(self):
        result, calls = self.run_script(FAKE_RELEASE="exists")
        self.assertRefused(result, calls, "release %s exists" % TAG)

    def test_failed_release_lookup_is_refused(self):
        result, calls = self.run_script(FAKE_RELEASE="error")
        self.assertRefused(result, calls, "could not look up release %s" % TAG)

    # -- the commit and the tarball must fit ---------------------------------

    def test_lock_for_another_version_is_refused(self):
        lock = os.path.join(self.repo, "ha-yt-to-upnp", "artefacts.lock")
        with open(lock) as f:
            text = f.read()
        with open(lock, "w") as f:
            f.write(text.replace("version: %s" % VERSION, "version: 1.2.2"))
        self.commit("stale lock")
        result, calls = self.run_script()
        self.assertRefused(result, calls, "artefacts.lock is for version 1.2.2")

    def test_receiver_source_changed_after_the_build_is_refused(self):
        self.write("src/main.rs", "changed\n")
        self.commit("receiver change")
        for args in ((), ("--dry-run",)):
            with self.subTest(args=args):
                result, calls = self.run_script(*args)
                self.assertRefused(result, calls, "receiver sources changed since the tarball's commit")

    def test_adapter_changed_after_the_build_is_refused(self):
        self.write("adapter/raumfeld_adapter.py", "changed\n")
        self.commit("adapter change")
        result, calls = self.run_script()
        self.assertRefused(result, calls, "adapter changed since the tarball's commit")

    def test_app_folder_changed_after_the_build_is_refused(self):
        self.write("ha-yt-to-upnp/rootfs/run", "changed\n")
        self.commit("app change")
        result, calls = self.run_script()
        self.assertRefused(result, calls, "app folder changed since the tarball's commit")

    def test_docs_in_the_app_folder_may_change_after_the_build(self):
        self.write("ha-yt-to-upnp/DOCS.md", "docs\n")
        self.write("ha-yt-to-upnp/translations/README.md", "docs\n")
        self.commit("docs")
        result, _ = self.run_script("--dry-run")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_build_info_commit_unknown_here_is_refused(self):
        self.commit_release("f" * 40)
        result, calls = self.run_script("--dry-run")
        self.assertRefused(result, calls, "is not in this repository")

    def test_tracked_artefacts_in_the_app_folder_are_refused(self):
        self.write("ha-yt-to-upnp/artefacts/BUILD-INFO", "x\n")
        self.git("add", "-f", "ha-yt-to-upnp/artefacts/BUILD-INFO")
        self.git("commit", "-qm", "artefacts")
        result, calls = self.run_script("--dry-run")
        self.assertRefused(result, calls, "ha-yt-to-upnp/artefacts is tracked")

    # -- only from master, on top of origin's ---------------------------------

    def test_other_branch_is_refused(self):
        self.git("checkout", "-q", "-b", "feature")
        result, calls = self.run_script()
        self.assertRefused(result, calls, "publish from master")

    def test_head_not_on_top_of_origin_master_is_refused(self):
        self.git("checkout", "-q", "-b", "side", self.first)
        self.write("other", "x\n")
        side = self.commit("side")
        self.git("checkout", "-q", "master")
        result, calls = self.run_script(FAKE_ORIGIN_MASTER=side)
        self.assertRefused(result, calls, "origin/master is not an ancestor of HEAD")

    def test_master_pushed_first_warns_and_still_publishes_in_order(self):
        result, calls = self.run_script(FAKE_ORIGIN_MASTER="HEAD")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("origin/master already has this commit", result.stderr)
        steps = [c.split(" --")[0] for c in self.remote_calls(calls)
                 if c.startswith(("git push", "gh release", "curl "))]
        self.assertEqual(steps, [
            "gh release view %s" % TAG,
            "git push origin %s:refs/tags/%s" % (self.built, TAG),
            "gh release create %s %s" % (TAG, self.tarball),
            "gh release download %s" % TAG,
            "gh release edit %s" % TAG,
            "curl -fsSL",
        ])

    # -- the uploaded asset is checked twice ----------------------------------

    def test_tampered_draft_asset_is_never_published(self):
        tampered = os.path.join(self.tmp, "tampered")
        with open(tampered, "wb") as f:
            f.write(b"tampered")
        result, calls = self.run_script(FAKE_DRAFT_ASSET=tampered)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the draft's asset has sha256", result.stderr)
        self.assertFalse([c for c in calls if c.startswith("gh release edit")], calls)

    def test_public_download_is_retried_while_the_asset_is_not_there_yet(self):
        with open(self.env["FAKE_CURL_FAILS_FILE"], "w") as f:
            f.write("2")
        result, calls = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len([c for c in calls if c.startswith("curl ")]), 3)

    def test_public_download_that_keeps_failing_says_the_release_is_published(self):
        with open(self.env["FAKE_CURL_FAILS_FILE"], "w") as f:
            f.write("99")
        result, _ = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is published; only the check of its public download failed", result.stderr)


if __name__ == "__main__":
    unittest.main()
