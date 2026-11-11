"""Tests of the stream_format helpers in ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh: the
MPD httpd encoder lines and the stream's MIME type for each format the schema allows.

Runs on the host, no Docker (bash and, for the MPD check, mpd if installed):
python3 -I scripts/ha_mpd_conf_test.py
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
APP = os.path.join(ROOT, "ha-yt-to-upnp")
COMMON = os.path.join(APP, "rootfs", "usr", "lib", "ytcast", "common.sh")
TEMPLATE = os.path.join(APP, "rootfs", "etc", "ytcast", "mpd.conf.template")

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
            template = f.read()
        with tempfile.TemporaryDirectory() as d:
            for sub in ("music", "playlists"):
                os.mkdir(os.path.join(d, sub))
            # httpd only opens its port once something plays: a second of silence.
            with wave.open(os.path.join(d, "music", "silence.wav"), "wb") as w:
                w.setnchannels(2), w.setsampwidth(2), w.setframerate(48000)
                w.writeframes(b"\0" * 48000 * 4)
            control, stream = free_port(), free_port()
            conf = (template.replace("@STREAM_ENCODER@", encoder)
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


if __name__ == "__main__":
    unittest.main()
