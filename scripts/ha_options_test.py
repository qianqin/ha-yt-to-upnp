"""Tests of the app's option check (ha-yt-to-upnp/rootfs/usr/lib/ytcast/check_options.py).

Runs on the host, no Docker: python3 -I scripts/ha_options_test.py
"""
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "ha-yt-to-upnp")
CHECKER = os.path.join(APP, "rootfs", "usr", "lib", "ytcast", "check_options.py")

# No __pycache__ in the app folder: the build refuses untracked files there.
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("check_options", CHECKER)
check_options = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_options)

VALID = {
    "cast_name": "Living Room",
    "raumfeld_room_udn": "uuid:00000000-0000-0000-0000-000000000001",
    "raumfeld_speaker_udn": "uuid:00000000-0000-0000-0000-000000000002",
    "log_level": "info",
}


def config_schema():
    """The schema block of config.yaml as {option: type string}."""
    schema, inside = {}, False
    with open(os.path.join(APP, "config.yaml")) as f:
        for line in f:
            if re.match(r"^schema:\s*$", line):
                inside = True
                continue
            if inside:
                if line.strip().startswith("#") or not line.strip():
                    continue
                m = re.match(r"^  ([a-z_]+): (.+)$", line.rstrip("\n"))
                if not m:
                    break
                value = m.group(2)
                if value.startswith("'") and value.endswith("'"):
                    value = value[1:-1].replace("''", "'")
                schema[m.group(1)] = value
    return schema


def problem(options):
    return check_options.problem(options)


class SchemaInStep(unittest.TestCase):
    def test_checker_uses_exactly_the_config_yaml_schema(self):
        self.assertTrue(config_schema())
        self.assertEqual(check_options.SCHEMA, config_schema())


class Valid(unittest.TestCase):
    def test_required_options_only(self):
        self.assertIsNone(problem(VALID))

    def test_all_options(self):
        options = dict(VALID, stream_host="192.0.2.10", start_gate_seconds=0,
                       stuck_seconds=300, log_level="debug")
        self.assertIsNone(problem(options))

    def test_unset_optional_options(self):
        for unset in (None, ""):
            options = dict(VALID, stream_host=unset, start_gate_seconds=None)
            self.assertIsNone(problem(options))

    def test_bounds(self):
        self.assertIsNone(problem(dict(VALID, start_gate_seconds=60, stuck_seconds=1)))
        self.assertIsNone(problem(dict(VALID, cast_name="x" * 60)))


class Invalid(unittest.TestCase):
    def assertProblem(self, options, option):
        message = problem(options)
        self.assertIsNotNone(message)
        self.assertIn("'%s'" % option, message)

    def test_missing_required(self):
        for key in VALID:
            for unset in (None, ""):
                options = dict(VALID, **{key: unset})
                self.assertProblem(options, key)
            options = dict(VALID)
            del options[key]
            self.assertProblem(options, key)

    def test_cast_name(self):
        for bad in ("a<b", "a>b", "a&b", 'a"b', "a\nb", "a\tb", "a\x00b", "x" * 61, "name\n", 5):
            self.assertProblem(dict(VALID, cast_name=bad), "cast_name")

    def test_udns(self):
        for key in ("raumfeld_room_udn", "raumfeld_speaker_udn"):
            for bad in ("00000000-0000", "uuid:", "uuid:a b", "uuid:" + "a" * 65, "uuid:x\n", "UUID:x"):
                self.assertProblem(dict(VALID, **{key: bad}), key)

    def test_stream_host(self):
        for bad in ("host:8000", "http://host", "a b", "x" * 254, "host\n"):
            self.assertProblem(dict(VALID, stream_host=bad), "stream_host")

    def test_log_level(self):
        for bad in ("INFO", "trace", "warn", 1):
            self.assertProblem(dict(VALID, log_level=bad), "log_level")

    def test_timings(self):
        for key, low, high in (("start_gate_seconds", 0, 60), ("stuck_seconds", 1, 300)):
            for bad in (low - 1, high + 1, "10", 10.5, True):
                self.assertProblem(dict(VALID, **{key: bad}), key)

    def test_message_names_the_rule_not_the_value(self):
        message = problem(dict(VALID, cast_name="secret<"))
        self.assertNotIn("secret", message)
        self.assertIn("^[^<>&", message)


class CommandLine(unittest.TestCase):
    def run_checker(self, content):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "options.json")
            with open(path, "w") as f:
                f.write(content)
            return subprocess.run([sys.executable, "-I", CHECKER, path],
                                  capture_output=True, text=True)

    def test_valid_options_exit_0_silently(self):
        r = self.run_checker(json.dumps(VALID))
        self.assertEqual((r.returncode, r.stdout, r.stderr), (0, "", ""))

    def test_invalid_option_exits_1_with_one_line(self):
        r = self.run_checker(json.dumps(dict(VALID, stuck_seconds=0)))
        self.assertEqual(r.returncode, 1)
        self.assertEqual(r.stderr, "")
        self.assertEqual(len(r.stdout.splitlines()), 1)
        self.assertIn("'stuck_seconds'", r.stdout)

    def test_unreadable_options(self):
        for content in ("", "not json", "[]"):
            r = self.run_checker(content)
            self.assertEqual(r.returncode, 1)
            self.assertEqual(len(r.stdout.splitlines()), 1)


if __name__ == "__main__":
    unittest.main()
