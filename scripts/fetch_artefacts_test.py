"""Tests of the app's artefacts stage (ha-yt-to-upnp/fetch-artefacts.sh).

Runs the script with sh, as the Dockerfile does, against a local http server
(the test-only ARTEFACTS_URL to 127.0.0.1); no network, no Docker:

  python3 -I scripts/fetch_artefacts_test.py

On the Mac, sh is bash and tar is bsdtar. The Dockerfile runs it as root with
dash and GNU tar; to test that too, run this file in the cached smoke base
image (see docs/DEVOPS.md, Smoke test).
"""
import functools
import hashlib
import http.server
import io
import os
import stat
import subprocess
import tarfile
import tempfile
import threading
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "ha-yt-to-upnp", "fetch-artefacts.sh")
VERSION = "1.2.3"
ENTRIES = ("BUILD-INFO", "ytcast-lite", "deno", "adapter", "bgutil")
MAX_FILESIZE = 512 * 1024 * 1024  # curl's --max-filesize 512M
# Modes with setuid or setgid bits, for files and folders of the artefacts.
SPECIAL = {"ytcast-lite": 0o4755, "deno": 0o2755, "adapter": 0o2755, "adapter/raumfeld_adapter.py": 0o6755}
URL_REFUSED = "ARTEFACTS_URL must be https://"
# For the URL checks: a value that passes must not reach the network, where a
# host like 192.0.2.1 makes curl wait for its connect timeout.
FAKE_CURL = """#!/bin/sh
echo "curl $*" >> "$FAKE_CURL_LOG"
exit 7
"""


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    """Files from the served folder; /nolength/<n>: n zero bytes without a
    Content-Length (the end is where the connection closes)."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        if not self.path.startswith("/nolength/"):
            return super().do_GET()
        left = int(self.path.rsplit("/", 1)[1])
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        chunk = bytes(1 << 20)
        while left > 0:
            self.wfile.write(chunk[:min(left, len(chunk))])
            left -= len(chunk)


class QuietServer(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # curl hangs up on the download over the limit


def tarball_bytes(modes):
    """A release tarball (artefacts/<entry>) with the given modes."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        def add(name, mode, data=None):
            info = tarfile.TarInfo("artefacts/" + name)
            info.mode = mode
            if data is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        add("", 0o755)
        add("BUILD-INFO", 0o644, b"built: now\n")
        add("ytcast-lite", modes.get("ytcast-lite", 0o755), b"receiver")
        add("deno", modes.get("deno", 0o755), b"deno")
        add("adapter", modes.get("adapter", 0o755))
        add("adapter/raumfeld_adapter.py", modes.get("adapter/raumfeld_adapter.py", 0o644), b"adapter")
        add("bgutil", 0o755)
    return buf.getvalue()


class FetchArtefactsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._served = tempfile.TemporaryDirectory()
        cls.served = cls._served.name
        handler = functools.partial(QuietHandler, directory=cls.served)
        cls.server = QuietServer(("127.0.0.1", 0), handler)
        cls.port = cls.server.server_address[1]
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls._served.cleanup()

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.realpath(self._tmp.name)
        self.stage = os.path.join(self.tmp, "stage")
        self.out = os.path.join(self.tmp, "out")
        os.makedirs(self.stage)
        with open(os.path.join(self.stage, "config.yaml"), "w") as f:
            f.write('name: x\nversion: "%s"\n' % VERSION)
        self.fake_bin = os.path.join(self.tmp, "bin")
        self.curl_log = os.path.join(self.tmp, "curl.log")
        os.makedirs(self.fake_bin)
        with open(os.path.join(self.fake_bin, "curl"), "w") as f:
            f.write(FAKE_CURL)
        os.chmod(os.path.join(self.fake_bin, "curl"), 0o755)

    def tearDown(self):
        self._tmp.cleanup()

    def serve(self, name, data):
        """Serves data at /<name>; returns its URL."""
        with open(os.path.join(self.served, name), "wb") as f:
            f.write(data)
        return self.url(name)

    def url(self, name):
        return "http://127.0.0.1:%d/%s" % (self.port, name)

    def write_lock(self, sha256, url="https://github.com/owner/repo/releases/download/v1.2.3/a.tar.gz"):
        with open(os.path.join(self.stage, "artefacts.lock"), "w") as f:
            f.write("version: %s\nurl: %s\nsha256: %s\n" % (VERSION, url, sha256))

    def run_fetch(self, artefacts_url=None, fake_curl=False, tmpdir=None):
        env = {"PATH": os.environ["PATH"], "TMPDIR": tmpdir or self.tmp, "FAKE_CURL_LOG": self.curl_log}
        if fake_curl:
            env["PATH"] = self.fake_bin + os.pathsep + env["PATH"]
        if artefacts_url is not None:
            env["ARTEFACTS_URL"] = artefacts_url
        return subprocess.run(["sh", SCRIPT, self.stage, self.out], env=env,
                              capture_output=True, text=True, timeout=120)

    def assertFails(self, result, message):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("fetch-artefacts: error: " + message, result.stderr)

    def assertUrlRefused(self, url):
        if os.path.exists(self.curl_log):
            os.remove(self.curl_log)
        self.assertFails(self.run_fetch(url, fake_curl=True), URL_REFUSED)
        self.assertFalse(os.path.exists(self.curl_log), "curl was called")

    def assertNoSpecialBits(self):
        for name in SPECIAL:
            mode = os.lstat(os.path.join(self.out, name)).st_mode
            self.assertEqual(mode & (stat.S_ISUID | stat.S_ISGID), 0, "%s: %o" % (name, mode))

    # -- the test-only ARTEFACTS_URL -----------------------------------------

    def test_http_to_this_machine_is_used_with_the_lock_sha256(self):
        data = tarball_bytes({})
        self.write_lock(hashlib.sha256(data).hexdigest())
        result = self.run_fetch(self.serve("genuine.tar.gz", data))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("matches artefacts.lock", result.stdout)
        self.assertEqual(sorted(os.listdir(self.out)), sorted(ENTRIES))

    def test_https_passes_the_url_check_and_keeps_https_only(self):
        self.write_lock("0" * 64)
        result = self.run_fetch("https://192.0.2.1/a.tar.gz", fake_curl=True)
        self.assertFails(result, "download of https://192.0.2.1/a.tar.gz failed")
        with open(self.curl_log) as f:
            call = f.read()
        self.assertIn("--proto =https --proto-redir =https ", call)
        # -q first: no .curlrc of the build's user; --globoff: [] and {} are no URL ranges.
        self.assertTrue(call.startswith("curl -q "), call)
        self.assertIn(" --globoff ", call)

    def test_url_glob_and_backslash_characters_are_refused(self):
        self.write_lock("0" * 64)
        for url in ("https://192.0.2.1/a[1-3].tar.gz", "https://192.0.2.1/{a,b}.tar.gz",
                    "https://192.0.2.1/a]b", "https://192.0.2.1/a}b", "https://192.0.2.1\\x/a.tar.gz"):
            with self.subTest(url=url):
                self.assertUrlRefused(url)

    def test_the_lock_url_is_logged_as_it_is(self):
        # echo in dash (and the Mac's sh) would stop at \c.
        url = "https://192.0.2.1/a\\cb.tar.gz"
        self.write_lock("0" * 64, url=url)
        result = self.run_fetch(fake_curl=True)
        self.assertIn("fetch-artefacts: %s\n" % url, result.stdout)
        self.assertFails(result, "download of %s failed" % url)

    def test_http_to_this_machine_allows_only_http(self):
        self.write_lock("0" * 64)
        for url in ("http://127.0.0.1:8080/a.tar.gz", "http://host.docker.internal:8080/a/b-1.2.3_x.tar.gz"):
            with self.subTest(url=url):
                self.assertFails(self.run_fetch(url, fake_curl=True), "download of %s failed" % url)
                with open(self.curl_log) as f:
                    self.assertIn("--proto =http --proto-redir =http ", f.readlines()[-1])

    # -- the lock -------------------------------------------------------------

    def test_lock_without_a_valid_sha256_is_refused(self):
        for sha256 in ("0" * 63, "0" * 65, "A" * 64, "0" * 64 + " x", "x" + "0" * 64, ""):
            with self.subTest(sha256=sha256):
                self.write_lock(sha256)
                self.assertFails(self.run_fetch(fake_curl=True), "artefacts.lock has no valid sha256")

    def test_user_info_is_refused(self):
        self.write_lock("0" * 64)
        for url in ("http://127.0.0.1:1@192.0.2.1:8080/a.tar.gz",
                    "http://host.docker.internal:1@192.0.2.1/a.tar.gz",
                    "http://127.0.0.1@192.0.2.1:8080/a.tar.gz",
                    "https://user@192.0.2.1/a.tar.gz",
                    "https://user:secret@192.0.2.1:8443/a.tar.gz",
                    "https://192.0.2.1:443@192.0.2.2/a.tar.gz"):
            with self.subTest(url=url):
                self.assertUrlRefused(url)

    def test_only_the_whole_value_is_matched(self):
        self.write_lock("0" * 64)
        for url in ("http://127.0.0.1:%d/a.tar.gz?x" % self.port,
                    "http://127.0.0.1:%d/a.tar.gz x" % self.port,
                    "xhttp://127.0.0.1:%d/a.tar.gz" % self.port,
                    "http://192.0.2.1/http://127.0.0.1:%d/a.tar.gz" % self.port,
                    "http://127.0.0.1:%d/a.tar.gz\nhttp://192.0.2.1/" % self.port,
                    "https://192.0.2.1/a.tar.gz\nuser@192.0.2.2",
                    "https://192.0.2.1/a.tar.gz\n"):
            with self.subTest(url=url):
                self.assertUrlRefused(url)

    def test_backslash_escapes_cannot_cut_the_value_short(self):
        # dash's echo (and the Mac's sh) would stop printing at \c and turn \n
        # into a newline, so the check would see only the part before it.
        self.write_lock("0" * 64)
        for url in ("https://192.0.2.1\\c@192.0.2.2/a.tar.gz",
                    "http://127.0.0.1:%d/a.tar.gz\\c@192.0.2.1" % self.port,
                    "http://127.0.0.1:%d/a.tar.gz\\nhttp://192.0.2.1/" % self.port):
            with self.subTest(url=url):
                self.assertUrlRefused(url)

    def test_other_hosts_and_schemes_are_refused(self):
        self.write_lock("0" * 64)
        for url in ("http://192.0.2.1:8080/a.tar.gz", "http://127.0.0.1/a.tar.gz",
                    "ftp://192.0.2.1/a.tar.gz", "file:///etc/passwd", "https://"):
            with self.subTest(url=url):
                self.assertUrlRefused(url)

    # -- the download ---------------------------------------------------------

    def test_sha256_mismatch_stops_the_build(self):
        self.write_lock("0" * 64)
        result = self.run_fetch(self.serve("other.tar.gz", tarball_bytes({})))
        self.assertFails(result, "sha256 mismatch")
        self.assertFalse(os.path.exists(os.path.join(self.out, "ytcast-lite")))

    def test_a_download_over_512_mib_is_refused(self):
        # Sparse: no disk space for the served file. curl stops at the length header.
        path = os.path.join(self.served, "huge.tar.gz")
        with open(path, "wb") as f:
            f.truncate(MAX_FILESIZE + 1)
        self.addCleanup(os.remove, path)
        self.write_lock("0" * 64)
        result = self.run_fetch(self.url("huge.tar.gz"))
        self.assertFails(result, "download of %s failed" % self.url("huge.tar.gz"))

    def test_a_download_over_512_mib_without_a_length_is_refused(self):
        # curl 7.88 (bookworm) checks --max-filesize only against a Content-Length.
        self.write_lock("0" * 64)
        url = self.url("nolength/%d" % (MAX_FILESIZE + 1))
        result = self.run_fetch(url)
        self.assertFails(result, "download of %s failed" % url)

    def test_a_download_of_exactly_512_mib_without_a_length_is_taken(self):
        self.write_lock("0" * 64)
        result = self.run_fetch(self.url("nolength/%d" % MAX_FILESIZE))
        self.assertFails(result, "sha256 mismatch")

    def test_a_download_of_exactly_512_mib_is_taken(self):
        # Downloaded in full, so it reaches the sha256 check.
        path = os.path.join(self.served, "limit.tar.gz")
        with open(path, "wb") as f:
            f.truncate(MAX_FILESIZE)
        self.addCleanup(os.remove, path)
        self.write_lock("0" * 64)
        result = self.run_fetch(self.url("limit.tar.gz"))
        self.assertFails(result, "sha256 mismatch")

    # -- no setuid or setgid bit survives -----------------------------------

    def test_special_bits_from_the_release_tarball_are_removed(self):
        data = tarball_bytes(SPECIAL)
        self.write_lock(hashlib.sha256(data).hexdigest())
        result = self.run_fetch(self.serve("special.tar.gz", data))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNoSpecialBits()

    def test_special_bits_gained_while_unpacking_are_removed(self):
        # tar itself drops the archive's setuid and setgid bits, but on Linux
        # a folder made in a setgid folder (here the unpack folder in TMPDIR)
        # is setgid too.
        data = tarball_bytes({})
        self.write_lock(hashlib.sha256(data).hexdigest())
        tmpdir = os.path.join(self.tmp, "setgid")
        os.makedirs(tmpdir)
        os.chmod(tmpdir, 0o2755)
        result = self.run_fetch(self.serve("plain.tar.gz", data), tmpdir=tmpdir)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNoSpecialBits()

    def test_special_bits_from_the_build_context_are_removed(self):
        for name in ENTRIES:
            path = os.path.join(self.stage, name)
            if name in ("adapter", "bgutil"):
                os.makedirs(path)
            else:
                with open(path, "w") as f:
                    f.write(name)
        with open(os.path.join(self.stage, "adapter", "raumfeld_adapter.py"), "w") as f:
            f.write("adapter")
        for name, mode in SPECIAL.items():
            path = os.path.join(self.stage, name)
            os.chmod(path, mode)
            self.assertEqual(stat.S_IMODE(os.lstat(path).st_mode), mode, "setup: %s" % name)
        result = self.run_fetch()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("artefacts/ from the build context", result.stdout)
        self.assertNoSpecialBits()


if __name__ == "__main__":
    unittest.main()
