"""Pure logic: config, line-in detection, decisions, DIDL/SOAP building, idle timer."""

import io
import time
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
                           ("STREAM_URL", "ftp://x/y"), ("LOG_LEVEL", "LOUD"),
                           ("RAUMFELD_ROOM_UDN", "uuid:x\r\nX-Injected: 1"),
                           ("RAUMFELD_SPEAKER_UDN", "speaker"),
                           ("RAUMFELD_SPEAKER_UDN", "uuid:" + "a" * 65),
                           ("RAUMFELD_ROOM_UDN", "uuid:abc\nST: ssdp:all")]:
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


ZONES = (b"<?xml version='1.0'?><zoneConfig><zones>"
         b"<zone udn='uuid:00000000-0000-0000-0000-0000000000b1'>"
         b"<room udn='uuid:00000000-0000-0000-0000-0000000000c1' powerState='ACTIVE'/></zone>"
         b"<zone udn=\"uuid:00000000-0000-0000-0000-0000000000b2\">"
         b"<room udn=\"%s\" powerState=\"MANUAL_STANDBY\"><renderer udn='x'/></room></zone>"
         b"</zones></zoneConfig>" % ROOM_UDN.encode())


class ParsingTest(unittest.TestCase):
    def test_zone_and_power_of_room(self):
        self.assertEqual(ra.zone_of_room(ZONES, ROOM_UDN), "uuid:00000000-0000-0000-0000-0000000000b2")
        self.assertEqual(ra.room_power_state(ZONES, ROOM_UDN), "MANUAL_STANDBY")

    def test_room_udn_prefix_does_not_match(self):
        self.assertIsNone(ra.zone_of_room(ZONES, ROOM_UDN[:-1]))
        self.assertIsNone(ra.room_power_state(ZONES, ROOM_UDN[:-1]))

    def test_invalid_zone_udn_is_rejected(self):
        bad = ZONES.replace(b"uuid:00000000-0000-0000-0000-0000000000b2", b"uuid:x\r\nST: ssdp:all")
        with self.assertRaises(ra.RaumfeldError):
            ra.zone_of_room(bad, ROOM_UDN)

    def test_doctype_and_entities_are_refused(self):
        for doc in (b"<!DOCTYPE z [<!ENTITY a 'b'>]><zoneConfig>&a;</zoneConfig>",
                    b"<!DOCTYPE zoneConfig><zoneConfig/>"):  # bare DOCTYPE, no entity
            with self.subTest(doc=doc), self.assertRaisesRegex(ra.RaumfeldError, "DOCTYPE/ENTITY"):
                ra.zone_of_room(doc, ROOM_UDN)
        with self.assertRaisesRegex(ra.RaumfeldError, "DOCTYPE/ENTITY"):
            ra.avtransport_control_url(b"<!DOCTYPE root><root/>", "http://127.0.0.1/d.xml")

    def test_entity_outside_a_doctype_is_malformed(self):
        with self.assertRaisesRegex(ra.RaumfeldError, "malformed XML"):
            ra.zone_of_room(b"<?xml version='1.0'?><!ENTITY a 'b'><zoneConfig/>", ROOM_UDN)

    def test_doctype_in_utf16_is_refused(self):
        doc = ('<?xml version="1.0" encoding="UTF-16"?><!DOCTYPE z [<!ENTITY a "bbb">]>'
               '<zoneConfig><zones><zone udn="uuid:a"><room udn="%s"/>&a;</zone></zones>'
               '</zoneConfig>' % ROOM_UDN)
        # UTF-16 always contains NUL bytes for ASCII markup, so the NUL check stops it first.
        for codec in ("utf-16", "utf-16-le"):
            with self.subTest(codec=codec), self.assertRaisesRegex(ra.RaumfeldError, "NUL bytes"):
                ra.zone_of_room(doc.encode(codec), ROOM_UDN)

    def test_utf16_without_doctype_is_refused_by_the_nul_check(self):
        doc = '<?xml version="1.0" encoding="UTF-16"?><zoneConfig/>'
        for data in (doc.encode("utf-16"), doc.encode("utf-16-le"), b"<zoneConfig>\x00</zoneConfig>"):
            with self.subTest(data=data), self.assertRaisesRegex(ra.RaumfeldError, "NUL bytes"):
                ra.zone_of_room(data, ROOM_UDN)

    def test_doctype_is_refused_by_the_parser_not_a_byte_scan(self):
        # The parser, not a byte pattern, enforces the rule: a tab after DOCTYPE still counts.
        with self.assertRaisesRegex(ra.RaumfeldError, "DOCTYPE/ENTITY"):
            ra.zone_of_room(b"<?xml version='1.0'?>\n<!DOCTYPE\tz [<!ENTITY a 'b'>]><z>&a;</z>",
                            ROOM_UDN)

    def test_utf8_device_description_still_parses(self):
        desc = ("<?xml version='1.0' encoding='utf-8'?><root xmlns='urn:schemas-upnp-org:device-1-0'>"
                "<device><friendlyName>Räum</friendlyName><serviceList><service>"
                "<serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>"
                "<controlURL>/ctrl</controlURL></service></serviceList></device></root>").encode()
        self.assertEqual(ra.avtransport_control_url(desc, "http://127.0.0.1:1/d.xml"),
                         "http://127.0.0.1:1/ctrl")

    def test_unclosed_tags_fail_fast(self):
        started = time.monotonic()
        with self.assertRaises(ra.RaumfeldError):
            ra.zone_of_room(b"<zoneConfig>" + b"<zone udn='uuid:a'" * 50000, ROOM_UDN)
        self.assertLess(time.monotonic() - started, 1)

    def test_device_url_check(self):
        ok = "http://127.0.0.1:49152/desc.xml"
        self.assertEqual(ra.check_device_url(ok, "127.0.0.1"), ok)
        for url in ("file:///dev/zero", "ftp://127.0.0.1/x", "https://127.0.0.1/x",
                    "http:///x", "http://127.0.0.2:49152/desc.xml", "http://evil.invalid/x"):
            with self.subTest(url=url), self.assertRaises(ra.RaumfeldError):
                ra.check_device_url(url, "127.0.0.1")


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

    def test_device_supplied_error_text_is_logged_escaped(self):
        self.ctl.fail = ra.RaumfeldError("HTTP 500 Boom\n2026-01-01 INFO forged line")
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            self.f.on_mpd_state("play")
        self.assertNotIn("\n", logs.records[0].getMessage())
        self.assertIn("\\n", logs.records[0].getMessage())

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
