"""Checks the app's options before any service starts.

The Supervisor validates options against config.yaml's schema when they are
saved, but /data/options.json can still hold values from an older schema or be
written by other means; the receiver writes cast_name unescaped into XML, so
nothing starts on options that break the schema.

usage: python3 -I check_options.py /data/options.json
Exits 0 silently if the options are valid; otherwise prints one line naming the
option and the rule (never the value) and exits 1.
"""
import json
import re
import sys

# The schema exactly as written in config.yaml (scripts/ha_options_test.py keeps
# the two in step). A trailing "?" marks an optional option.
SCHEMA = {
    "cast_name": 'match(^[^<>&"\\x00-\\x1f]{1,60}$)',
    "raumfeld_room_udn": "match(^uuid:[0-9A-Za-z-]{1,64}$)",
    "raumfeld_speaker_udn": "match(^uuid:[0-9A-Za-z-]{1,64}$)",
    "stream_host": "match(^[0-9A-Za-z.-]{1,253}$)?",
    "start_gate_seconds": "int(0,60)?",
    "stuck_seconds": "int(1,300)?",
    "log_level": "list(debug|info|warning|error)",
}


def _rule_problem(rule, value):
    """Why value breaks rule (a schema type without "?"), or None."""
    kind, arg = re.fullmatch(r"(match|int|list)\((.*)\)", rule).groups()
    if kind == "match":
        # fullmatch: "$" alone would also accept a trailing newline.
        if isinstance(value, str) and re.fullmatch(arg, value):
            return None
        return "must match %s" % arg
    if kind == "int":
        low, high = (int(n) for n in arg.split(","))
        if type(value) is int and low <= value <= high:
            return None
        return "must be a whole number from %d to %d" % (low, high)
    if value in arg.split("|"):
        return None
    return "must be one of %s" % ", ".join(arg.split("|"))


def problem(options):
    """The first problem with the options as one line, or None if they are valid."""
    if not isinstance(options, dict):
        return "options are not a JSON object"
    for name, rule in SCHEMA.items():
        optional = rule.endswith("?")
        value = options.get(name)
        if value is None or value == "":
            if optional:
                continue
            return "option '%s' is required but not set" % name
        why = _rule_problem(rule.rstrip("?"), value)
        if why:
            return "option '%s' is invalid: %s" % (name, why)
    return None


def main(path):
    try:
        with open(path) as f:
            options = json.load(f)
    except (OSError, ValueError) as e:
        message = "cannot read the options from %s (%s)" % (path, type(e).__name__)
    else:
        message = problem(options)
    if message:
        print(message)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
