"""Tests of the PoToken helper's s6 run script
(ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/potoken/run) and its Deno entry file
(ha-yt-to-upnp/rootfs/opt/bgutil/ytcast-entry.ts): the Supervisor's TZ reaches the helper,
the start logs the time zone (and warns about a zero UTC offset), Deno runs the entry file
with every restriction, and the entry file sets an HTTP user agent that names the real
platform.

The run script is run with bash on a copy whose helper path points to a temporary folder;
bashio's log functions and s6-setuidgid are stubbed (the stub records the command it would
run, nothing is started).

EntryFileSetsTheUserAgent runs the entry file under Deno with the run script's restrictions,
bgutil's src/main.ts replaced by a stub that makes one request through bgutil's axios to a
local listener (no internet). It needs deno and bgutil's node_modules: by default Homebrew's
deno and the assembled folder's dist/ha-yt-to-upnp/artefacts/bgutil (skipped without them);
scripts/smoke-ha-app.sh also runs it in the image, against the installed /opt/bgutil as the
potoken user (YTCAST_TEST_BGUTIL_DIR, YTCAST_TEST_ENTRY, YTCAST_TEST_DENO,
YTCAST_TEST_DENO_USER).

Runs on the host, no Docker: python3 -I scripts/ha_potoken_run_test.py
"""
import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "ha-yt-to-upnp")
RUN = os.path.join(APP, "rootfs", "etc", "s6-overlay", "s6-rc.d", "potoken", "run")
ENTRY = os.path.join(APP, "rootfs", "opt", "bgutil", "ytcast-entry.ts")
DOCKERFILE = os.path.join(APP, "Dockerfile")

sys.dont_write_bytecode = True

STUB = """#!/bin/sh
python3 -I -c 'import json, sys; json.dump(sys.argv[2:], open(sys.argv[1], "w"))' \
    "$ARGV_OUT" "$@"
"""

# Deno's flags as the app has run the helper since 0.3: none may get lost.
DENO_FLAGS = [
    "--no-prompt", "--frozen", "--cached-only",
    "--allow-net=127.0.0.1:4416,www.youtube.com:443,www.google.com:443,jnn-pa.googleapis.com:443",
    "--allow-env", "--deny-write", "--deny-run", "--deny-ffi", "--deny-sys", "--deny-import",
]


def run_script(tz, date_output=None):
    """(argv given to s6-setuidgid, log lines) of the run script, TZ set to tz (None: unset).
    With date_output, a date stub on PATH prints it instead of the real offset."""
    with tempfile.TemporaryDirectory() as tmp:
        app = os.path.join(tmp, "bgutil")
        os.mkdir(app)
        with open(RUN) as f:
            text = f.read()
        assert text.count("readonly app=/opt/bgutil\n") == 1, "run script names its folder once"
        script = os.path.join(tmp, "run")
        with open(script, "w") as f:
            f.write(text.replace("readonly app=/opt/bgutil\n", "readonly app=%s\n" % app))
        bin_dir = os.path.join(tmp, "bin")
        os.mkdir(bin_dir)
        with open(os.path.join(bin_dir, "s6-setuidgid"), "w") as f:
            f.write(STUB)
        os.chmod(os.path.join(bin_dir, "s6-setuidgid"), 0o755)
        if date_output is not None:
            with open(os.path.join(bin_dir, "date"), "w") as f:
                f.write("#!/bin/sh\necho '%s'\n" % date_output)
            os.chmod(os.path.join(bin_dir, "date"), 0o755)
        argv_out = os.path.join(tmp, "argv.json")
        env = {"PATH": bin_dir + ":/usr/bin:/bin", "ARGV_OUT": argv_out}
        if tz is not None:
            env["TZ"] = tz
        prelude = ('bashio::log.info() { echo "INFO $*"; }; '
                   'bashio::log.warning() { echo "WARNING $*"; }; '
                   'bashio::exit.nok() { echo "nok: $*"; exit 1; }; '
                   'source "$1"')
        r = subprocess.run(["bash", "-c", prelude, "bash", script], env=env,
                           capture_output=True, text=True)
        if r.returncode != 0:
            raise AssertionError("run script failed (%d): %s%s" % (r.returncode, r.stdout, r.stderr))
        with open(argv_out) as f:
            argv = json.load(f)
        return argv, r.stdout.splitlines()


def helper_env(argv):
    """The environment env -i gives the helper, as {name: value}."""
    assert argv[:3] == ["potoken", "env", "-i"], argv[:3]
    env = {}
    for arg in argv[3:]:
        if "=" not in arg or arg.startswith("/"):
            break
        name, value = arg.split("=", 1)
        env[name] = value
    return env


def deno_args(argv):
    i = argv.index("/usr/local/bin/deno")
    return argv[i + 1:]


class TimeZoneReachesTheHelper(unittest.TestCase):
    def test_tz_from_the_supervisor_is_passed(self):
        argv, _ = run_script("Europe/Berlin")
        self.assertEqual(helper_env(argv).get("TZ"), "Europe/Berlin")

    def test_unset_tz_is_passed_empty(self):
        argv, _ = run_script(None)
        self.assertEqual(helper_env(argv).get("TZ"), "")

    def test_environment_is_still_only_the_fixed_set_plus_tz(self):
        argv, _ = run_script("Europe/Berlin")
        self.assertEqual(set(helper_env(argv)),
                         {"DENO_DIR", "DENO_NO_UPDATE_CHECK", "NO_COLOR", "HOME", "PATH", "TZ"})


class TimeZoneIsLogged(unittest.TestCase):
    def test_name_and_offset_are_logged(self):
        _, log = run_script("Europe/Berlin")
        info = [l for l in log if l.startswith("INFO ") and "time zone" in l]
        self.assertEqual(len(info), 1, log)
        self.assertRegex(info[0], r"Europe/Berlin \(UTC\+0[12]00\)")
        self.assertFalse([l for l in log if l.startswith("WARNING ")], log)

    def test_negative_offset_is_no_warning(self):
        _, log = run_script("America/New_York")
        self.assertTrue([l for l in log if re.search(r"America/New_York \(UTC-0[45]00\)", l)], log)
        self.assertFalse([l for l in log if l.startswith("WARNING ")], log)

    def test_zero_offset_or_no_tz_warns(self):
        for tz in (None, "", "UTC", "Etc/GMT"):
            with self.subTest(tz=tz):
                _, log = run_script(tz)
                warnings = [l for l in log if l.startswith("WARNING ")]
                self.assertEqual(len(warnings), 1, log)
                self.assertIn("YouTube may reject PoTokens", warnings[0])
                self.assertIn("UTC offset", warnings[0])
                self.assertTrue([l for l in log if l.startswith("INFO ") and "UTC+0000" in l], log)

    def test_negative_zero_offset_warns(self):
        _, log = run_script("Europe/Berlin", date_output="-0000")
        self.assertTrue([l for l in log if l.startswith("INFO ") and "UTC-0000" in l], log)
        warnings = [l for l in log if l.startswith("WARNING ")]
        self.assertEqual(len(warnings), 1, log)
        self.assertIn("YouTube may reject PoTokens", warnings[0])


class InvalidTimeZoneIsIgnored(unittest.TestCase):
    """A TZ that is not a plain zone name (a POSIX rule, a :path, a ../ walk, more than one
    line) is not passed on: the helper gets an empty TZ, as without one, and the warning."""

    BAD = ["Europe/Berlin\nUTC", ":/etc/localtime", ":Europe/Berlin", "../x", "Europe/../../x",
           "/etc/localtime", "Europe//Berlin", "Europe/Berlin/", "CET-1CEST,M3.5.0,M10.5.0/3",
           "Europe/Berlin x", "$(id)"]
    GOOD = ["Europe/Berlin", "America/Argentina/Buenos_Aires", "Etc/GMT+5", "Etc/GMT-14", "UTC",
            "America/Port-au-Prince"]

    def test_invalid_tz_is_passed_empty_with_the_warning(self):
        for tz in self.BAD:
            with self.subTest(tz=tz):
                argv, log = run_script(tz)
                self.assertEqual(helper_env(argv).get("TZ"), "")
                self.assertTrue([l for l in log if l.startswith("INFO ") and "not set" in l], log)
                self.assertEqual(len([l for l in log if l.startswith("WARNING ")]), 1, log)
                # The value itself stays out of the log.
                self.assertFalse([l for l in log if tz.split("\n")[0] in l], log)

    def test_zone_names_are_passed(self):
        for tz in self.GOOD:
            with self.subTest(tz=tz):
                self.assertEqual(helper_env(run_script(tz)[0]).get("TZ"), tz)


class DenoRunsTheEntryFile(unittest.TestCase):
    def test_entry_file_instead_of_bgutil_main(self):
        args = deno_args(run_script("Europe/Berlin")[0])
        self.assertEqual(args[0], "run")
        self.assertIn("ytcast-entry.ts", args)
        self.assertNotIn("src/main.ts", args)
        script = args.index("ytcast-entry.ts")
        self.assertEqual(args[script + 1:], ["--host", "127.0.0.1", "--port", "4416"])

    def test_every_deno_restriction_is_kept(self):
        argv, _ = run_script("Europe/Berlin")
        args = deno_args(argv)
        flags = args[1:args.index("ytcast-entry.ts")]
        app = helper_env(argv)["DENO_DIR"][:-len("/.deno")]
        self.assertEqual(sorted(flags), sorted(DENO_FLAGS + ["--allow-read=%s/node_modules" % app]))


class EntryFile(unittest.TestCase):
    """Static guards on the entry file's text; its behaviour is tested by
    EntryFileSetsTheUserAgent and by the full smoke test's 120-character token."""

    def setUp(self):
        with open(ENTRY) as f:
            self.text = f.read()

    def test_user_agent_names_the_real_platform(self):
        self.assertIn("navigator.platform", self.text)
        self.assertIn('import axios from "axios";', self.text)
        self.assertRegex(self.text, r'headers\.set\("user-agent", userAgent\)')
        # bgutils-js's own format; a full Chrome user agent makes YouTube reject the tokens.
        self.assertIn("`Mozilla/5.0 (X11; ${navigator.platform}) AppleWebKit/537.36(KHTML, like Gecko)`",
                      self.text)

    def test_starts_bgutil_after_setting_the_user_agent(self):
        self.assertTrue(self.text.rstrip().endswith('await import("./src/main.ts");'))
        self.assertLess(self.text.index("interceptors"), self.text.index('import("./src/main.ts")'))

    def test_no_platform_or_time_zone_spoofing(self):
        for word in ("Macintosh", "Windows", "Intl.DateTimeFormat", "getTimezoneOffset",
                     "Object.defineProperty"):
            self.assertNotIn(word, self.text)


BGUTIL_DIR = os.environ.get("YTCAST_TEST_BGUTIL_DIR",
                            os.path.join(ROOT, "dist", "ha-yt-to-upnp", "artefacts", "bgutil"))
TEST_ENTRY = os.environ.get("YTCAST_TEST_ENTRY", ENTRY)
DENO = os.environ.get("YTCAST_TEST_DENO") or shutil.which("deno")
DENO_USER = os.environ.get("YTCAST_TEST_DENO_USER")

# Stands in for bgutil's src/main.ts: one GET through bgutil's axios (the bare import
# resolves to the same instance the entry file configured) to the listener on Deno.args[0].
MAIN_STUB = """import axios from "axios";
await axios.get(`http://127.0.0.1:${Deno.args[0]}/`, { headers: { "x-platform": navigator.platform } });
"""


class _Listener(http.server.BaseHTTPRequestHandler):
    seen = []

    def do_GET(self):
        self.seen.append((self.headers.get("user-agent"), self.headers.get("x-platform")))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *args):
        pass


@unittest.skipUnless(DENO and os.path.isdir(os.path.join(BGUTIL_DIR, "node_modules", "axios")),
                     "needs deno and bgutil's node_modules (scripts/build-ha-app.sh)")
class EntryFileSetsTheUserAgent(unittest.TestCase):
    def test_requests_carry_the_platform_user_agent(self):
        _Listener.seen = []
        server = http.server.HTTPServer(("127.0.0.1", 0), _Listener)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        port = server.server_address[1]
        tmp = tempfile.mkdtemp()
        try:
            os.chmod(tmp, 0o755)
            os.mkdir(os.path.join(tmp, "src"))
            for name in ("package.json", "deno.json", "deno.lock"):
                shutil.copy(os.path.join(BGUTIL_DIR, name), tmp)
            os.symlink(os.path.join(BGUTIL_DIR, "node_modules"), os.path.join(tmp, "node_modules"))
            shutil.copy(TEST_ENTRY, os.path.join(tmp, "ytcast-entry.ts"))
            with open(os.path.join(tmp, "src", "main.ts"), "w") as f:
                f.write(MAIN_STUB)
            for root, dirs, files in os.walk(tmp):
                for name in files:
                    os.chmod(os.path.join(root, name), 0o644)
                for name in dirs:
                    os.chmod(os.path.join(root, name), 0o755)
            deno_dir = os.path.join(tmp, ".deno")
            os.mkdir(deno_dir)
            os.chmod(deno_dir, 0o777)
            # The run script's environment and restrictions; only --allow-net names the listener.
            cmd = ["env", "-i", "DENO_DIR=" + deno_dir, "DENO_NO_UPDATE_CHECK=1", "NO_COLOR=1",
                   "HOME=/nonexistent", "PATH=/usr/bin:/bin", "TZ=Europe/Berlin",
                   DENO, "run", "--no-prompt", "--frozen", "--cached-only",
                   "--allow-net=127.0.0.1:%d" % port,
                   "--allow-read=%s/node_modules,%s/node_modules" % (tmp, BGUTIL_DIR),
                   "--allow-env", "--deny-write", "--deny-run", "--deny-ffi", "--deny-sys",
                   "--deny-import", "ytcast-entry.ts", str(port)]
            if DENO_USER:
                cmd = ["/command/s6-setuidgid", DENO_USER] + cmd
            r = subprocess.run(cmd, cwd=tmp, capture_output=True, text=True, timeout=120)
        finally:
            server.shutdown()
            server.server_close()
            shutil.rmtree(tmp)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(_Listener.seen), 1, r.stderr)
        user_agent, platform = _Listener.seen[0]
        self.assertTrue(platform, "the stub reports navigator.platform")
        self.assertEqual(user_agent,
                         "Mozilla/5.0 (X11; %s) AppleWebKit/537.36(KHTML, like Gecko)" % platform)


class ImageInstallsTheEntryFile(unittest.TestCase):
    def test_read_only_for_the_helper(self):
        with open(DOCKERFILE) as f:
            text = f.read()
        self.assertRegex(text, r"chmod 0644 /opt/bgutil/ytcast-entry\.ts")
        # rootfs/ is copied after bgutil: the folder's mode is set again with it.
        self.assertRegex(text, r"chmod a\+rX,go-w /opt/bgutil[ \n]")
        boundary = text.index("# smoke-quick-cache-boundary")
        self.assertNotIn("ytcast-entry", text[:boundary])


def dockerfiles():
    """The app's Dockerfile and, once assembled, dist/'s copy of it."""
    assembled = os.path.join(ROOT, "dist", "ha-yt-to-upnp", "Dockerfile")
    return [DOCKERFILE] + ([assembled] if os.path.exists(assembled) else [])


def _targets_bgutil(path, recursive):
    path = path.rstrip("/") or "/"
    return (path == "/opt/bgutil" or path.startswith("/opt/bgutil/")
            or (recursive and path in ("/", "/opt")))


def bgutil_ownership_changes(text):
    """Dockerfile instructions that hand /opt/bgutil (or a file in it) to another owner or
    group: chown/chgrp in a RUN, or COPY/ADD --chown into it."""
    found = []
    for instruction in re.sub(r"\\\n", " ", text).splitlines():
        words = instruction.split()
        if words and words[0].upper() in ("COPY", "ADD") \
                and any(w.startswith("--chown") for w in words) \
                and _targets_bgutil(words[-1], True):
            found.append(instruction.strip())
            continue
        for command in re.split(r"&&|\|\||;|\|", instruction):
            args = command.split()
            for i, arg in enumerate(args):
                if os.path.basename(arg) not in ("chown", "chgrp"):
                    continue
                rest = args[i + 1:]
                recursive = any(a.startswith("-") and ("R" in a or a == "--recursive") for a in rest)
                if any(_targets_bgutil(a, recursive) for a in rest if a.startswith("/")):
                    found.append(command.strip())
    return found


class BgutilStaysRootOwned(unittest.TestCase):
    """The helper runs as potoken: owning its code under /opt/bgutil would let it rewrite it."""

    def test_no_chown_or_chgrp_on_opt_bgutil(self):
        for path in dockerfiles():
            with open(path) as f:
                self.assertEqual(bgutil_ownership_changes(f.read()), [], path)

    def test_the_check_catches_ownership_changes(self):
        for mutant in [
            "RUN chown potoken /opt/bgutil/ytcast-entry.ts",
            "RUN chmod 0644 /opt/bgutil/ytcast-entry.ts && chown potoken:potoken /opt/bgutil/ytcast-entry.ts",
            "RUN true \\\n    && chgrp -R potoken /opt/bgutil",
            "RUN chown -R potoken /opt",
            "COPY --chown=potoken rootfs/opt/bgutil/ /opt/bgutil/",
        ]:
            self.assertTrue(bgutil_ownership_changes(mutant), mutant)

    def test_the_check_leaves_other_paths_alone(self):
        for fine in [
            "RUN chmod a+rX,go-w /opt/bgutil && chmod 0644 /opt/bgutil/ytcast-entry.ts",
            "RUN chown potoken /var/lib/potoken",
            "RUN chown potoken /opt/other",
            "COPY --from=artefacts /artefacts/bgutil/ /opt/bgutil/",
        ]:
            self.assertEqual(bgutil_ownership_changes(fine), [], fine)


if __name__ == "__main__":
    unittest.main()
