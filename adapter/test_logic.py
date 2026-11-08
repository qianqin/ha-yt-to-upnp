"""Pure logic: config, line-in detection, decisions, DIDL/SOAP building, idle timer."""

import io
import unittest
import xml.etree.ElementTree as ET
from contextlib import redirect_stderr

import raumfeld_adapter as ra
from fakes import LINE_IN_URI, ROOM_UDN, SPEAKER_UDN, STREAM_URL

REQUIRED = {
    "RAUMFELD_ROOM_UDN": ROOM_UDN,
    "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
    "STREAM_URL": STREAM_URL,
}


class ConfigTest(unittest.TestCase):
    def test_defaults(self):
        c = ra.Config.from_env(REQUIRED)
        self.assertEqual(c.room_udn, ROOM_UDN)
        self.assertEqual(c.speaker_udn, SPEAKER_UDN)
        self.assertEqual(c.stream_url, STREAM_URL)
        self.assertEqual(c.stream_mime, "audio/mpeg")
        self.assertTrue(c.stream_metadata)
        self.assertEqual(c.idle_seconds, 300)
        self.assertEqual((c.mpd_host, c.mpd_port), ("localhost", 6600))
        self.assertEqual(c.log_level, "INFO")

    def test_overrides(self):
        env = dict(REQUIRED, STREAM_MIME="audio/flac", STREAM_METADATA="0", IDLE_MINUTES="1.5",
                   MPD_HOST="mpd.invalid", MPD_PORT="6601", LOG_LEVEL="debug")
        c = ra.Config.from_env(env)
        self.assertEqual(c.stream_mime, "audio/flac")
        self.assertFalse(c.stream_metadata)
        self.assertEqual(c.idle_seconds, 90)
        self.assertEqual((c.mpd_host, c.mpd_port), ("mpd.invalid", 6601))
        self.assertEqual(c.log_level, "DEBUG")

    def test_missing_required_names_every_missing_var(self):
        with self.assertRaises(ra.ConfigError) as cm:
            ra.Config.from_env({"STREAM_URL": STREAM_URL})
        msg = str(cm.exception)
        self.assertIn("RAUMFELD_ROOM_UDN", msg)
        self.assertIn("RAUMFELD_SPEAKER_UDN", msg)
        self.assertNotIn("STREAM_URL", msg)

    def test_invalid_values(self):
        for key, value in [("MPD_PORT", "x"), ("MPD_PORT", "70000"), ("IDLE_MINUTES", "-1"),
                           ("IDLE_MINUTES", "soon"), ("STREAM_METADATA", "maybe"),
                           ("STREAM_URL", "ftp://x/y"), ("LOG_LEVEL", "LOUD")]:
            with self.subTest(key=key, value=value), self.assertRaises(ra.ConfigError):
                ra.Config.from_env(dict(REQUIRED, **{key: value}))

    def test_main_exits_nonzero_with_clear_message(self):
        err = io.StringIO()
        with redirect_stderr(err):
            code = ra.main(env={})
        self.assertNotEqual(code, 0)
        self.assertIn("STREAM_URL", err.getvalue())
        self.assertIn("RAUMFELD_ROOM_UDN", err.getvalue())


class LineInTest(unittest.TestCase):
    def test_double_encoded_line_in(self):
        self.assertTrue(ra.is_line_in(LINE_IN_URI))

    def test_other_uris(self):
        self.assertFalse(ra.is_line_in(STREAM_URL))
        self.assertFalse(ra.is_line_in(""))
        self.assertFalse(ra.is_line_in("dlna-playsingle://x?iid=0%2FMusic%2F1"))
        self.assertFalse(ra.is_line_in("http://x/Line%20In.mp3"))


class DecisionTest(unittest.TestCase):
    def s(self, state, uri):
        return ra.ZoneStatus(state, uri)

    def test_play_decisions(self):
        d = ra.decide_play
        self.assertEqual(d(None, STREAM_URL), ra.TAKE_OVER)
        self.assertEqual(d(self.s("PLAYING", STREAM_URL), STREAM_URL), ra.ALREADY_PLAYING)
        self.assertEqual(d(self.s("TRANSITIONING", STREAM_URL), STREAM_URL), ra.ALREADY_PLAYING)
        self.assertEqual(d(self.s("STOPPED", STREAM_URL), STREAM_URL), ra.TAKE_OVER)
        self.assertEqual(d(self.s("PLAYING", LINE_IN_URI), STREAM_URL), ra.LINE_IN_ACTIVE)
        self.assertEqual(d(self.s("TRANSITIONING", LINE_IN_URI), STREAM_URL), ra.LINE_IN_ACTIVE)
        self.assertEqual(d(self.s("STOPPED", LINE_IN_URI), STREAM_URL), ra.TAKE_OVER)
        self.assertEqual(d(self.s("PLAYING", "http://other.invalid/x"), STREAM_URL), ra.TAKE_OVER)

    def test_release_decisions(self):
        d = ra.decide_release
        self.assertFalse(d(None, STREAM_URL))
        self.assertTrue(d(self.s("PLAYING", STREAM_URL), STREAM_URL))
        self.assertTrue(d(self.s("TRANSITIONING", STREAM_URL), STREAM_URL))
        # A paused MPD stalls the stream; the renderer then often sits STOPPED/PAUSED on our URI.
        self.assertTrue(d(self.s("STOPPED", STREAM_URL), STREAM_URL))
        self.assertTrue(d(self.s("PAUSED_PLAYBACK", STREAM_URL), STREAM_URL))
        self.assertFalse(d(self.s("PLAYING", LINE_IN_URI), STREAM_URL))
        self.assertFalse(d(self.s("STOPPED", "http://other.invalid/x"), STREAM_URL))


class MetadataTest(unittest.TestCase):
    def test_didl(self):
        didl = ra.build_didl(STREAM_URL, "audio/flac")
        root = ET.fromstring(didl)
        ns = {"d": "urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/",
              "dc": "http://purl.org/dc/elements/1.1/",
              "upnp": "urn:schemas-upnp-org:metadata-1-0/upnp/"}
        res = root.find("d:item/d:res", ns)
        self.assertEqual(res.get("protocolInfo"), "http-get:*:audio/flac:*")
        self.assertEqual(res.text, STREAM_URL)
        self.assertEqual(root.find("d:item/dc:title", ns).text, "YouTube Music cast")
        self.assertEqual(root.find("d:item/upnp:class", ns).text, "object.item.audioItem.musicTrack")

    def test_soap_envelope_escapes_arguments(self):
        body = ra.soap_envelope("SetAVTransportURI", [("InstanceID", "0"), ("CurrentURIMetaData", "<a b='&'/>")])
        self.assertIn("&lt;a b=", body)
        root = ET.fromstring(body)
        meta = [el for el in root.iter() if el.tag == "CurrentURIMetaData"][0]
        self.assertEqual(meta.text, "<a b='&'/>")
        action = [el for el in root.iter() if el.tag.endswith("}SetAVTransportURI")][0]
        self.assertEqual(action.tag, "{urn:schemas-upnp-org:service:AVTransport:1}SetAVTransportURI")


class FakeClock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


class RecordingController:
    def __init__(self):
        self.calls = []
        self.fail = None

    def ensure_playing(self):
        self.calls.append("ensure_playing")
        if self.fail:
            raise self.fail

    def release(self):
        self.calls.append("release")
        if self.fail:
            raise self.fail


class FollowerTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.f = ra.Follower(self.ctl, idle_seconds=300, clock=self.clock)

    def test_play_drives_speaker_without_deadline(self):
        self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_idle_timeout_releases_once(self):
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("pause")
        self.clock.now += 120
        self.f.on_mpd_state("stop")  # continuing idle does not restart the timer
        self.assertEqual(self.f.seconds_until_deadline(), 180)
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.clock.now += 180
        self.f.tick()
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["ensure_playing", "release"])
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_resume_before_timeout_cancels(self):
        self.f.on_mpd_state("stop")
        self.clock.now += 299
        self.f.on_mpd_state("play")
        self.clock.now += 10
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["ensure_playing"])

    def test_errors_are_logged_not_raised(self):
        self.ctl.fail = ra.DeviceNotFound(SPEAKER_UDN)
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play")
        self.ctl.fail = OSError("connection refused")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing"])

    def test_failed_play_is_retried_from_tick_while_mpd_still_plays(self):
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play")
        self.assertEqual(self.f.seconds_until_deadline(), ra.PLAY_RETRY_S)
        self.clock.now += ra.PLAY_RETRY_S - 1
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.ctl.fail = None
        self.clock.now += 1
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing"])
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_play_retry_is_dropped_when_mpd_stops(self):
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play")
        self.f.on_mpd_state("stop")
        self.assertEqual(self.f.seconds_until_deadline(), 300)

    def test_failed_release_is_retried_after_another_idle_period(self):
        self.f.on_mpd_state("stop")
        self.clock.now += 300
        self.ctl.fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.tick()
        self.assertEqual(self.f.seconds_until_deadline(), 300)
        self.ctl.fail = None
        self.clock.now += 300
        self.f.tick()
        self.assertEqual(self.ctl.calls, ["release", "release"])


if __name__ == "__main__":
    unittest.main()
