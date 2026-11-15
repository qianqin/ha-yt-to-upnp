"""Tests of the PoToken helper's s6 run script
(ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/potoken/run) and its Deno entry file
(ha-yt-to-upnp/rootfs/opt/bgutil/ytcast-entry.ts): the Supervisor's TZ reaches the helper,
the start logs the time zone (and warns about a zero UTC offset), Deno runs the entry file
with every restriction, and the entry file sets an HTTP user agent that names the real
platform.

The run script is run with bash on a copy whose helper path points to a temporary folder;
bashio's log functions and s6-setuidgid are stubbed (the stub records the command it would
run, nothing is started).

Runs on the host with bash 4.3 or later (as in the image; not macOS's 3.2),
no Docker: python3 -I scripts/ha_potoken_run_test.py
"""
import json
import os
import re
import subprocess
import sys
import tempfile
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


def run_script(tz):
    """(argv given to s6-setuidgid, log lines) of the run script, TZ set to tz (None: unset)."""
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


class ImageInstallsTheEntryFile(unittest.TestCase):
    def test_read_only_for_the_helper(self):
        with open(DOCKERFILE) as f:
            text = f.read()
        self.assertRegex(text, r"chmod 0644 /opt/bgutil/ytcast-entry\.ts")


if __name__ == "__main__":
    unittest.main()
