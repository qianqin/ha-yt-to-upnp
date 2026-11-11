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
    "stream_format": "wav",
    "log_level": "info",
}

# raumfeld_host as the adapter takes it (adapter/raumfeld_adapter.py, _raumfeld_host):
# an IPv4 address (a value of digits and dots must be one; no leading zeros) or a
# DNS name of 1-63 character labels, 253 characters at most.
GOOD_HOSTS = ["192.0.2.20", "0.0.0.0", "255.255.255.255", "raumfeld-host.local", "speaker",
              "a" * 63, ".".join(["a" * 63] * 3) + "." + "a" * 61, "1-2.3", "x1.example",
              "1.2.3.4.example", "9host"]
BAD_HOSTS = ["256.1.1.1", "1.2.3", "1.2.3.4.5", "01.2.3.4", "1..2.3", "192.0.2.20.",
             "host:47365", "http://host", "host/path", "a b", "-host", "host-", "a.-b", "a_b",
             "a" * 64, "x.", ".x", ".".join(["a" * 63] * 4), "h\u00f6st", "1.2.3.4/24"]
# The adapter strips surrounding whitespace; the option must not have any.
PADDED_HOSTS = [" host", "host\n", "192.0.2.20 "]


def config_schema():
    """The schema block of config.yaml as {option: type string}."""
    return config_block("schema")


def config_block(block):
    """A top-level block of config.yaml (options or schema) as {option: value string}."""
    schema, inside = {}, False
    with open(os.path.join(APP, "config.yaml")) as f:
        for line in f:
            if re.match(r"^%s:\s*$" % block, line):
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


class DefaultsInStep(unittest.TestCase):
    def test_no_optional_option_has_a_null_default(self):
        # The Supervisor merges these defaults into the saved options and rejects
        # any None value as a missing required option, whatever the schema says:
        # an optional option listed as null could never be left empty.
        options, schema = config_block("options"), config_schema()
        self.assertTrue(options)
        self.assertLessEqual(set(options), set(schema))
        for name, value in options.items():
            if schema[name].endswith("?"):
                self.assertNotEqual(value, "null", name)

    def test_stream_format_defaults_to_wav(self):
        # WAV streams silence while MPD is held; FLAC sends nothing then, so the speaker
        # waits out the start gate.
        self.assertEqual(config_block("options").get("stream_format"), "wav")


class TranslationsInStep(unittest.TestCase):
    def test_every_option_has_a_name_and_description(self):
        with open(os.path.join(APP, "translations", "en.yaml")) as f:
            text = f.read()
        for name in config_schema():
            m = re.search(r"^  %s:\n    name: .+\n    description: .+" % name, text, re.M)
            self.assertIsNotNone(m, name)


class AdapterAgreesOnRaumfeldHost(unittest.TestCase):
    def test_option_accepts_exactly_what_the_adapter_accepts(self):
        sys.path.insert(0, os.path.join(ROOT, "adapter"))
        try:
            import raumfeld_adapter
        finally:
            sys.path.pop(0)
        for host in GOOD_HOSTS + BAD_HOSTS:
            try:
                raumfeld_adapter._raumfeld_host({"RAUMFELD_HOST": host})
                adapter_ok = True
            except raumfeld_adapter.ConfigError:
                adapter_ok = False
            option_ok = problem(dict(VALID, raumfeld_host=host)) is None
            self.assertEqual(option_ok, adapter_ok, host)


class Valid(unittest.TestCase):
    def test_required_options_only(self):
        self.assertIsNone(problem(VALID))

    def test_all_options(self):
        options = dict(VALID, stream_host="192.0.2.10", start_gate_seconds=0,
                       stuck_seconds=300, max_volume=50, log_level="debug")
        self.assertIsNone(problem(options))

    def test_unset_optional_options(self):
        for unset in (None, ""):
            options = dict(VALID, stream_host=unset, start_gate_seconds=None, max_volume=unset)
            self.assertIsNone(problem(options))

    def test_raumfeld_host(self):
        for host in GOOD_HOSTS:
            self.assertIsNone(problem(dict(VALID, raumfeld_host=host)), host)

    def test_raumfeld_host_makes_the_speaker_udn_optional(self):
        for unset in (None, ""):
            options = dict(VALID, raumfeld_host="192.0.2.20", raumfeld_speaker_udn=unset)
            self.assertIsNone(problem(options))
        options = dict(VALID, raumfeld_host="192.0.2.20")
        del options["raumfeld_speaker_udn"]
        self.assertIsNone(problem(options))

    def test_stream_formats(self):
        # flac stays valid: existing installs have it saved.
        for fmt in ("wav", "flac", "mp3"):
            self.assertIsNone(problem(dict(VALID, stream_format=fmt)), fmt)

    def test_bounds(self):
        self.assertIsNone(problem(dict(VALID, start_gate_seconds=60, stuck_seconds=1)))
        for volume in (1, 100):
            self.assertIsNone(problem(dict(VALID, max_volume=volume)), volume)
        self.assertIsNone(problem(dict(VALID, cast_name="x" * 60)))


class Invalid(unittest.TestCase):
    def assertProblem(self, options, option):
        message = problem(options)
        self.assertIsNotNone(message)
        self.assertIn("'%s'" % option, message)

    def test_missing_required(self):
        for key in VALID:
            if key == "raumfeld_speaker_udn":
                continue
            for unset in (None, ""):
                options = dict(VALID, **{key: unset})
                self.assertProblem(options, key)
            options = dict(VALID)
            del options[key]
            self.assertProblem(options, key)

    def test_speaker_udn_required_without_raumfeld_host(self):
        for unset in (None, ""):
            for host in (None, ""):
                message = problem(dict(VALID, raumfeld_speaker_udn=unset, raumfeld_host=host))
                self.assertIsNotNone(message)
                self.assertIn("'raumfeld_speaker_udn'", message)
                self.assertIn("raumfeld_host", message)
        options = dict(VALID)
        del options["raumfeld_speaker_udn"]
        self.assertProblem(options, "raumfeld_speaker_udn")

    def test_speaker_udn_checked_with_raumfeld_host(self):
        self.assertProblem(dict(VALID, raumfeld_host="192.0.2.20", raumfeld_speaker_udn="uuid:"),
                           "raumfeld_speaker_udn")

    def test_raumfeld_host_rejects_what_the_adapter_rejects(self):
        for bad in BAD_HOSTS + PADDED_HOSTS + [5]:
            self.assertProblem(dict(VALID, raumfeld_host=bad), "raumfeld_host")

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

    def test_stream_format(self):
        for bad in ("FLAC", "WAV", "wave", "ogg", "flac24", " wav", 1):
            self.assertProblem(dict(VALID, stream_format=bad), "stream_format")

    def test_log_level(self):
        for bad in ("INFO", "trace", "warn", 1):
            self.assertProblem(dict(VALID, log_level=bad), "log_level")

    def test_timings(self):
        for key, low, high in (("start_gate_seconds", 0, 60), ("stuck_seconds", 1, 300),
                               ("max_volume", 1, 100)):
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

    def test_missing_speaker_udn_without_raumfeld_host_exits_1(self):
        options = dict(VALID)
        del options["raumfeld_speaker_udn"]
        r = self.run_checker(json.dumps(options))
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(r.stdout.splitlines()), 1)
        self.assertIn("'raumfeld_speaker_udn'", r.stdout)

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
