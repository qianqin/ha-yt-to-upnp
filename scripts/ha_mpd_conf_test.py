"""Tests of MPD's configuration as ytcast-init writes it (ytcast_mpd_conf in
ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh, with the app's mpd.conf.template), and of the
stream_format helpers there: the httpd encoder lines and the stream's MIME type for each
format the schema allows; if mpd is installed, MPD starts with each format's configuration and
serves its stream with that MIME type (loopback, ephemeral ports).

Runs on the host with bash 4.3 or later (as in the image; not macOS's 3.2),
no Docker: python3 -I scripts/ha_mpd_conf_test.py
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import wave

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOTFS = os.path.join(ROOT, "ha-yt-to-upnp", "rootfs")
APP = os.path.join(ROOT, "ha-yt-to-upnp")
COMMON = os.path.join(ROOTFS, "usr", "lib", "ytcast", "common.sh")
TEMPLATE = os.path.join(ROOTFS, "etc", "ytcast", "mpd.conf.template")
INIT = os.path.join(ROOTFS, "etc", "s6-overlay", "scripts", "ytcast-init.sh")

sys.dont_write_bytecode = True


def helper(function, fmt):
    """(exit status, stdout) of a common.sh helper; bashio::exit.nok is stubbed."""
    script = ('bashio::exit.nok() { echo "nok: $*" >&2; exit 1; }; source "$1"; %s "$2"'
              % function)
    r = subprocess.run(["bash", "-c", script, "bash", COMMON, fmt],
                       capture_output=True, text=True)
    return r.returncode, r.stdout


def schema_formats():
    with open(os.path.join(APP, "config.yaml")) as f:
        return re.search(r"^  stream_format: list\(([^)]*)\)$", f.read(), re.M).group(1).split("|")


class StreamFormats(unittest.TestCase):
    def test_every_format_in_the_schema_has_an_encoder_and_a_mime_type(self):
        for fmt in schema_formats():
            with self.subTest(fmt=fmt):
                self.assertEqual(helper("ytcast_stream_encoder", fmt)[0], 0)
                self.assertEqual(helper("ytcast_stream_mime", fmt)[0], 0)

    def test_flac_and_mp3(self):
        self.assertEqual(helper("ytcast_stream_encoder", "flac"),
                         (0, 'encoder     "flac"\n    compression "5"'))
        self.assertEqual(helper("ytcast_stream_mime", "flac"), (0, "audio/flac\n"))
        self.assertEqual(helper("ytcast_stream_encoder", "mp3"),
                         (0, 'encoder     "lame"\n    bitrate     "320"'))
        self.assertEqual(helper("ytcast_stream_mime", "mp3"), (0, "audio/mpeg\n"))

    def test_an_unknown_format_stops(self):
        for fmt in ("wav", "ogg", ""):
            with self.subTest(fmt=fmt):
                self.assertNotEqual(helper("ytcast_stream_encoder", fmt)[0], 0)
                self.assertNotEqual(helper("ytcast_stream_mime", fmt)[0], 0)


@unittest.skipUnless(shutil.which("mpd"), "mpd not installed")
class MpdAcceptsTheConfig(unittest.TestCase):
    """The template with each format's encoder is a configuration MPD starts with, its
    stream answering with the format's MIME type. Loopback only, ephemeral ports."""

    def test_mpd_serves_each_format(self):
        for fmt in schema_formats():
            with self.subTest(fmt=fmt):
                self.serve(fmt)

    def serve(self, fmt):
        encoder = helper("ytcast_stream_encoder", fmt)[1]
        mime = helper("ytcast_stream_mime", fmt)[1].strip()
        with open(TEMPLATE) as f:
            template = f.read().rstrip("\n")  # as ytcast-init's $(< ...) reads it
        with tempfile.TemporaryDirectory() as d:
            for sub in ("music", "playlists"):
                os.mkdir(os.path.join(d, sub))
            # httpd only opens its port once something plays: a second of silence.
            with wave.open(os.path.join(d, "music", "silence.wav"), "wb") as w:
                w.setnchannels(2), w.setsampwidth(2), w.setframerate(48000)
                w.writeframes(b"\0" * 48000 * 4)
            control, stream = free_port(), free_port()
            built = mpd_conf(template, encoder)  # ytcast-init's own way
            self.assertEqual(built.returncode, 0, built.stderr)
            conf = (built.stdout
                    .replace('"/data/mpd/', '"%s/' % d)
                    .replace('port                "6600"', 'port                "%d"' % control)
                    .replace('port        "8000"', 'port        "%d"\n    bind_to_address '
                             '"127.0.0.1"' % stream))
            path = os.path.join(d, "mpd.conf")
            with open(path, "w") as f:
                f.write(conf)
            proc = subprocess.Popen(["mpd", "--no-daemon", path], stdout=subprocess.DEVNULL,
                                    stderr=subprocess.PIPE)
            try:
                mpd_commands(control, "update", "wait", "add silence.wav", "repeat 1", "play")
                ctype = http_content_type(stream)
            finally:
                proc.terminate()
                _, err = proc.communicate(timeout=10)
            self.assertEqual(ctype, mime, err.decode(errors="replace")[-500:])


def mpd_commands(port, *commands):
    """Send commands to MPD's control port, one connection each; "wait" waits for the
    database update."""
    for command in commands:
        if command == "wait":
            for _ in range(40):
                if "updating_db" not in mpd_commands(port, "status"):
                    break
                time.sleep(0.25)
            continue
        out = mpd_command(port, command)
    return out


def mpd_command(port, command):
    for _ in range(40):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                f = s.makefile("rwb", buffering=0)
                f.readline()
                f.write(command.encode() + b"\n")
                lines = []
                while True:
                    line = f.readline().decode().strip()
                    if line == "OK" or line.startswith("ACK") or not line:
                        return "\n".join(lines + [line])
                    lines.append(line)
        except OSError:
            time.sleep(0.25)
    raise AssertionError("MPD does not answer on port %d" % port)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_content_type(port):
    for _ in range(40):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=2) as s:
                s.sendall(b"GET /stream HTTP/1.0\r\n\r\n")
                head = b""
                while b"\r\n\r\n" not in head:
                    chunk = s.recv(1024)
                    if not chunk:
                        break
                    head += chunk
            for line in head.decode(errors="replace").split("\r\n"):
                if line.lower().startswith("content-type:"):
                    return line.split(":", 1)[1].strip()
            return None
        except OSError:
            time.sleep(0.25)
    return None


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
