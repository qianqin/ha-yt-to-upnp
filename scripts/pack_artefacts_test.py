"""Tests of the release packing (scripts/pack_artefacts.py).

Runs on the host, no Docker: python3 -I scripts/pack_artefacts_test.py
"""
import gzip
import hashlib
import io
import os
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pack_artefacts  # noqa: E402

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pack_artefacts.py")
MTIME = 1_700_000_000


def make_tree(root, file_mtime):
    """A small artefacts folder like the real one: binaries, a script, a tree with a relative link."""
    os.makedirs(os.path.join(root, "adapter"))
    os.makedirs(os.path.join(root, "bgutil", "node_modules", "pkg", "bin"))
    os.makedirs(os.path.join(root, "bgutil", "node_modules", ".bin"))
    files = {
        "ytcast-lite": (b"\x7fELF receiver", 0o755),
        "deno": (b"\x7fELF deno", 0o700),
        "BUILD-INFO": (b"built: now\n", 0o600),
        "adapter/raumfeld_adapter.py": (b"print('adapter')\n", 0o664),
        "bgutil/node_modules/pkg/bin/tool.js": (b"#!/usr/bin/env node\n", 0o775),
    }
    for rel, (data, mode) in files.items():
        path = os.path.join(root, rel)
        with open(path, "wb") as f:
            f.write(data)
        os.chmod(path, mode)
    os.symlink("../pkg/bin/tool.js", os.path.join(root, "bgutil", "node_modules", ".bin", "tool"))
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            os.utime(os.path.join(dirpath, name), (file_mtime, file_mtime), follow_symlinks=False)


class PackTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name

    def tearDown(self):
        self._tmp.cleanup()

    def pack(self, name="a", file_mtime=1_234_567):
        src = os.path.join(self.tmp, name, "artefacts")
        make_tree(src, file_mtime)
        out = os.path.join(self.tmp, name + ".tar.gz")
        digest = pack_artefacts.pack(src, out, MTIME)
        return out, digest

    def members(self, path):
        with tarfile.open(path, "r:gz") as t:
            return t.getmembers()

    def test_same_files_give_the_same_bytes_whatever_their_mtimes(self):
        a, digest_a = self.pack("a", file_mtime=1_000_000)
        b, digest_b = self.pack("b", file_mtime=2_000_000)
        with open(a, "rb") as fa, open(b, "rb") as fb:
            self.assertEqual(fa.read(), fb.read())
        self.assertEqual(digest_a, digest_b)

    def test_digest_is_the_sha256_of_the_file(self):
        out, digest = self.pack()
        with open(out, "rb") as f:
            self.assertEqual(digest, hashlib.sha256(f.read()).hexdigest())

    def test_entries_are_sorted_under_artefacts_with_fixed_metadata(self):
        out, _ = self.pack()
        members = self.members(out)
        names = [m.name for m in members]
        self.assertEqual(names, sorted(names))
        self.assertEqual(names[0], "artefacts")
        self.assertIn("artefacts/bgutil/node_modules/.bin/tool", names)
        for m in members:
            self.assertTrue(m.name == "artefacts" or m.name.startswith("artefacts/"), m.name)
            self.assertEqual((m.uid, m.gid, m.uname, m.gname, m.mtime), (0, 0, "", "", MTIME), m.name)

    def test_modes_are_normalised(self):
        out, _ = self.pack()
        modes = {m.name: stat.S_IMODE(m.mode) for m in self.members(out) if not m.issym()}
        self.assertEqual(modes["artefacts/ytcast-lite"], 0o755)
        self.assertEqual(modes["artefacts/deno"], 0o755)
        self.assertEqual(modes["artefacts/BUILD-INFO"], 0o644)
        self.assertEqual(modes["artefacts/adapter/raumfeld_adapter.py"], 0o644)
        self.assertEqual(modes["artefacts/bgutil/node_modules/pkg/bin/tool.js"], 0o755)
        self.assertEqual(modes["artefacts/adapter"], 0o755)

    def test_relative_links_inside_the_tree_are_kept_as_links(self):
        out, _ = self.pack()
        link = next(m for m in self.members(out) if m.name == "artefacts/bgutil/node_modules/.bin/tool")
        self.assertTrue(link.issym())
        self.assertEqual(link.linkname, "../pkg/bin/tool.js")

    def test_gzip_header_has_no_time_and_no_name(self):
        out, _ = self.pack()
        with open(out, "rb") as f:
            header = f.read(10)
        self.assertEqual(header[:2], b"\x1f\x8b")
        self.assertEqual(header[4:8], b"\0\0\0\0")  # MTIME
        self.assertEqual(header[3] & 0x08, 0)  # FNAME flag

    def test_a_link_out_of_the_tree_is_refused(self):
        for target in ("/etc/passwd", "../../outside", "../.."):
            with self.subTest(target=target):
                src = os.path.join(self.tmp, "bad-" + str(abs(hash(target))), "artefacts")
                make_tree(src, 1)
                os.symlink(target, os.path.join(src, "adapter", "escape"))
                with self.assertRaisesRegex(ValueError, "points out of"):
                    pack_artefacts.pack(src, src + ".tar.gz", MTIME)

    def test_a_link_through_another_link_out_of_the_tree_is_refused(self):
        # "up" points to the tree's root (inside). "esc" -> "up/../x" stays
        # inside when read as text (node_modules/x), but once "up" is followed
        # it is the tree's parent's x.
        src = os.path.join(self.tmp, "chain", "artefacts")
        make_tree(src, 1)
        modules = os.path.join(src, "bgutil", "node_modules")
        os.symlink("../..", os.path.join(modules, "up"))
        os.symlink("up/../x", os.path.join(modules, "esc"))
        with self.assertRaisesRegex(ValueError, "points out of"):
            pack_artefacts.pack(src, src + ".tar.gz", MTIME)

    def test_anything_but_files_dirs_and_links_is_refused(self):
        src = os.path.join(self.tmp, "fifo", "artefacts")
        make_tree(src, 1)
        os.mkfifo(os.path.join(src, "pipe"))
        with self.assertRaisesRegex(ValueError, "not a regular file"):
            pack_artefacts.pack(src, src + ".tar.gz", MTIME)


class ReleaseUrlTest(unittest.TestCase):
    def test_github_remotes(self):
        for remote in ("https://github.com/owner/repo.git", "https://github.com/owner/repo",
                       "https://github.com/owner/repo/", "git@github.com:owner/repo.git",
                       "ssh://git@github.com/owner/repo.git"):
            with self.subTest(remote=remote):
                self.assertEqual(pack_artefacts.github_repo(remote), ("owner", "repo"))

    def test_other_remotes_are_refused(self):
        for remote in ("https://gitlab.com/owner/repo.git", "https://github.com/owner",
                       "https://user:secret@github.com/owner/repo.git",
                       "https://x-access-token@github.com/owner/repo.git",
                       "http://github.com/owner/repo.git", "/local/path/repo.git",
                       "https://github.com/owner/repo/extra"):
            with self.subTest(remote=remote):
                with self.assertRaises(ValueError):
                    pack_artefacts.github_repo(remote)

    def test_asset_url(self):
        self.assertEqual(
            pack_artefacts.asset_url(("owner", "repo"), "0.4.0"),
            "https://github.com/owner/repo/releases/download/v0.4.0/ha-yt-to-upnp-artefacts-0.4.0-amd64.tar.gz")

    def test_bad_versions_are_refused(self):
        for version in ("0.4", "v0.4.0", "0.4.0-rc1", "0.4.0\n", ""):
            with self.subTest(version=version):
                with self.assertRaises(ValueError):
                    pack_artefacts.asset_name(version)


class LockTest(unittest.TestCase):
    def test_lock_file_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "artefacts.lock")
            pack_artefacts.write_lock(path, "0.4.0", "https://example.invalid/a.tar.gz", "ab" * 32)
            with open(path) as f:
                lines = [l for l in f.read().splitlines() if not l.startswith("#")]
        self.assertEqual(lines, ["version: 0.4.0", "url: https://example.invalid/a.tar.gz",
                                 "sha256: " + "ab" * 32])

    def test_bad_version_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                pack_artefacts.write_lock(os.path.join(tmp, "l"), "0.4", "https://x/a", "ab" * 32)

    def test_bad_digest_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                pack_artefacts.write_lock(os.path.join(tmp, "l"), "0.4.0", "https://x/a", "AB" * 32)


class CliTest(unittest.TestCase):
    def test_cli_writes_tarball_and_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = os.path.join(tmp, "artefacts")
            make_tree(src, 1)
            out_dir = os.path.join(tmp, "release")
            lock = os.path.join(tmp, "artefacts.lock")
            result = subprocess.run(
                [sys.executable, "-I", SCRIPT, "--version", "0.4.0", "--mtime", str(MTIME),
                 "--remote", "https://github.com/owner/repo.git", src, out_dir, lock],
                capture_output=True, text=True, check=True)
            tarball = os.path.join(out_dir, "ha-yt-to-upnp-artefacts-0.4.0-amd64.tar.gz")
            with open(tarball, "rb") as f:
                digest = hashlib.sha256(f.read()).hexdigest()
            with open(lock) as f:
                content = f.read()
            self.assertIn("sha256: %s\n" % digest, content)
            self.assertIn("url: https://github.com/owner/repo/releases/download/v0.4.0/"
                          "ha-yt-to-upnp-artefacts-0.4.0-amd64.tar.gz\n", content)
            self.assertIn(digest, result.stdout)


if __name__ == "__main__":
    unittest.main()
