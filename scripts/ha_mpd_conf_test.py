"""Tests of MPD's configuration as ytcast-init writes it (ytcast_mpd_conf in
ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh, with the app's mpd.conf.template).

Runs on the host with bash 4.3 or later (as in the image; not macOS's 3.2),
no Docker: python3 -I scripts/ha_mpd_conf_test.py
"""
import os
import subprocess
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOTFS = os.path.join(ROOT, "ha-yt-to-upnp", "rootfs")
COMMON = os.path.join(ROOTFS, "usr", "lib", "ytcast", "common.sh")
TEMPLATE = os.path.join(ROOTFS, "etc", "ytcast", "mpd.conf.template")
INIT = os.path.join(ROOTFS, "etc", "s6-overlay", "scripts", "ytcast-init.sh")
ENCODER = 'encoder     "flac"\n    compression "5"'
REFUSED = "/etc/ytcast/mpd.conf.template must name @STREAM_ENCODER@ exactly once"

# bashio's exit.nok as far as the test needs it: the message, then exit 1.
RUN = r'''bashio::exit.nok() { echo "exit.nok: $*" >&2; exit 1; }
source "$1"
ytcast_mpd_conf "$2" "$3"
echo "not reached after exit.nok" >&2
'''


def mpd_conf(template, encoder=ENCODER):
    return subprocess.run(["bash", "-c", RUN, "bash", COMMON, template, encoder],
                          capture_output=True, text=True, timeout=30)


class MpdConfTest(unittest.TestCase):
    def assertRefused(self, result):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("exit.nok: " + REFUSED, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_the_apps_template_gets_the_encoder(self):
        with open(TEMPLATE) as f:
            template = f.read().rstrip("\n")  # as ytcast-init's $(< ...) reads it
        result = mpd_conf(template)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("@STREAM_ENCODER@", result.stdout)
        self.assertEqual(result.stdout.count(ENCODER), 1)
        self.assertEqual(result.stdout, template.replace("@STREAM_ENCODER@", ENCODER) + "\n")

    def test_one_placeholder_is_replaced(self):
        result = mpd_conf("a\n  @STREAM_ENCODER@\nb")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "a\n  %s\nb\n" % ENCODER)

    def test_one_placeholder_on_a_long_line_is_replaced(self):
        line = "    # the stream's encoder, as stream_format says: @STREAM_ENCODER@ (here)"
        result = mpd_conf("a\n%s\nb" % line)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "a\n%s\nb\n" % line.replace("@STREAM_ENCODER@", ENCODER))

    def test_the_encoder_is_put_in_as_it_is(self):
        # bash 5.2's patsub_replacement turns an unquoted & into the match.
        for encoder in ('encoder "a&b"', "&", "\\&", "\\"):
            with self.subTest(encoder=encoder):
                result = mpd_conf("x @STREAM_ENCODER@ y", encoder)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "x %s y\n" % encoder)

    def test_no_placeholder_is_refused(self):
        for template in ("a\nb", "@STREAM_ENCODER", "STREAM_ENCODER@", ""):
            with self.subTest(template=template):
                self.assertRefused(mpd_conf(template))

    def test_two_placeholders_are_refused(self):
        for template in ("@STREAM_ENCODER@ @STREAM_ENCODER@",
                         "a\n@STREAM_ENCODER@@STREAM_ENCODER@\nb",
                         "@STREAM_ENCODER@\nb\n@STREAM_ENCODER@",
                         "@STREAM_ENCODER@\n@STREAM_ENCODER@\n@STREAM_ENCODER@"):
            with self.subTest(template=template):
                self.assertRefused(mpd_conf(template))

    def test_ytcast_init_writes_the_conf_with_it(self):
        with open(INIT) as f:
            init = f.read()
        self.assertIn('ytcast_mpd_conf "$(< /etc/ytcast/mpd.conf.template)" "${encoder}" > "${YTCAST_MPD_CONF}"',
                      init)
        self.assertNotIn("${template/", init)


if __name__ == "__main__":
    unittest.main()
