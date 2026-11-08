"""Pure logic: config, line-in detection, decisions, DIDL/SOAP building, Follower."""

import io
import time
import unittest
import xml.etree.ElementTree as ET
from contextlib import redirect_stderr
from unittest import mock

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
        self.assertEqual(c.poll_seconds, 5)
        self.assertEqual((c.mpd_host, c.mpd_port), ("localhost", 6600))
        self.assertEqual(c.log_level, "INFO")

    def test_overrides(self):
        env = dict(REQUIRED, STREAM_MIME="audio/flac", STREAM_METADATA="0",
                   SPEAKER_POLL_SECONDS="2.5", MPD_HOST="mpd.invalid", MPD_PORT="6601",
                   LOG_LEVEL="debug")
        c = ra.Config.from_env(env)
        self.assertEqual(c.stream_mime, "audio/flac")
        self.assertFalse(c.stream_metadata)
        self.assertEqual(c.poll_seconds, 2.5)
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
        for key, value in [("MPD_PORT", "x"), ("MPD_PORT", "70000"),
                           ("SPEAKER_POLL_SECONDS", "0"), ("SPEAKER_POLL_SECONDS", "-1"),
                           ("SPEAKER_POLL_SECONDS", "0.5"),
                           ("SPEAKER_POLL_SECONDS", "soon"), ("SPEAKER_POLL_SECONDS", "nan"),
                           ("SPEAKER_POLL_SECONDS", "inf"), ("STREAM_METADATA", "maybe"),
                           ("STREAM_URL", "ftp://x/y"), ("LOG_LEVEL", "LOUD"),
                           ("RAUMFELD_ROOM_UDN", "uuid:x\r\nX-Injected: 1"),
                           ("RAUMFELD_SPEAKER_UDN", "speaker"),
                           ("RAUMFELD_SPEAKER_UDN", "uuid:" + "a" * 65),
                           ("RAUMFELD_ROOM_UDN", "uuid:abc\nST: ssdp:all")]:
            with self.subTest(key=key, value=value), self.assertRaises(ra.ConfigError):
                ra.Config.from_env(dict(REQUIRED, **{key: value}))

    def test_idle_minutes_is_no_longer_a_setting(self):
        c = ra.Config.from_env(dict(REQUIRED, IDLE_MINUTES="soon"))  # ignored, not validated
        self.assertFalse(hasattr(c, "idle_seconds"))

    def test_main_warns_that_idle_minutes_is_ignored(self):
        with mock.patch.object(ra, "run") as run, \
                mock.patch.object(ra.logging, "basicConfig"), \
                self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            code = ra.main(env=dict(REQUIRED, IDLE_MINUTES="5"))
        self.assertEqual(code, 0)
        run.assert_called_once()
        self.assertTrue(any(r.levelname == "WARNING" and "IDLE_MINUTES" in r.getMessage()
                            for r in logs.records), logs.output)

    def test_main_does_not_warn_without_idle_minutes(self):
        with mock.patch.object(ra, "run"), mock.patch.object(ra.logging, "basicConfig"), \
                self.assertLogs("raumfeld_adapter", "INFO") as logs:
            ra.main(env=dict(REQUIRED))
        self.assertFalse(any("IDLE_MINUTES" in m for m in logs.output), logs.output)

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
        # An explicit cast wins over any other source, even one reporting PLAYING.
        for state in ("PLAYING", "TRANSITIONING", "STOPPED", "PAUSED_PLAYBACK"):
            for uri in (LINE_IN_URI, "http://other.invalid/x", ""):
                self.assertEqual(d(self.s(state, uri), STREAM_URL), ra.TAKE_OVER)

    def test_speaker_verdicts(self):
        j = ra.judge_speaker
        zone = "uuid:00000000-0000-0000-0000-0000000000b2"
        self.assertEqual(j("ACTIVE", zone, self.s("PLAYING", STREAM_URL), STREAM_URL), ra.OURS)
        self.assertEqual(j("ACTIVE", zone, self.s("TRANSITIONING", STREAM_URL), STREAM_URL), ra.OURS)
        for state in ("STOPPED", "PAUSED_PLAYBACK", "NO_MEDIA_PRESENT", ""):
            self.assertEqual(j("ACTIVE", zone, self.s(state, STREAM_URL), STREAM_URL), ra.NOT_PLAYING)
        self.assertEqual(j("MANUAL_STANDBY", zone, None, STREAM_URL), ra.STANDBY)
        self.assertEqual(j("AUTOMATIC_STANDBY", zone, None, STREAM_URL), ra.STANDBY)
        self.assertEqual(j(None, None, None, STREAM_URL), ra.NO_ZONE)
        self.assertEqual(j("ACTIVE", None, None, STREAM_URL), ra.NO_ZONE)
        for uri in (LINE_IN_URI, "http://other.invalid/x", ""):
            self.assertEqual(j("ACTIVE", zone, self.s("PLAYING", uri), STREAM_URL), ra.OTHER_SOURCE)
            self.assertEqual(j("ACTIVE", zone, self.s("STOPPED", uri), STREAM_URL), ra.OTHER_SOURCE)
        self.assertEqual(set(ra.TAKEN_AWAY), {ra.STANDBY, ra.NO_ZONE, ra.OTHER_SOURCE})


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
        self.decision = ra.TAKE_OVER
        self.verdict = ra.OURS

    def ensure_playing(self, wake=True):
        self.calls.append("ensure_playing" if wake else "ensure_playing(no wake)")
        if self.fail:
            raise self.fail
        return self.decision

    def check(self):
        self.calls.append("check")
        if self.fail:
            raise self.fail
        return self.verdict


class FollowerTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock)

    def advance(self, seconds):
        self.clock.now += seconds
        self.f.tick()

    def test_play_takes_over_and_schedules_a_poll(self):
        self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.advance(5)
        self.assertEqual(self.ctl.calls, ["ensure_playing", "check"])
        self.assertEqual(self.f.seconds_until_deadline(), 5)

    def test_owned_zone_is_polled_while_paused_and_stopped(self):
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("pause")
        self.advance(5)
        self.f.on_mpd_state("stop")
        self.advance(5)
        self.assertEqual(self.ctl.calls, ["ensure_playing", "check", "check"])

    def test_taken_away_while_stopped_just_lets_go(self):
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("stop")
        self.ctl.verdict = ra.STANDBY
        self.advance(5)
        self.assertEqual(self.f.take_mpd_requests(), [])  # no pause, no release
        self.assertIsNone(self.f.seconds_until_deadline())
        self.f.on_mpd_state("play")  # the next cast may wake the room
        self.assertEqual(self.ctl.calls, ["ensure_playing", "check", "ensure_playing"])

    def test_unserved_requests_are_requeued_first(self):
        self.f.on_mpd_state("play")
        self.ctl.verdict = ra.STANDBY
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.assertEqual(self.f.take_mpd_requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        # The pause went out, then the connection dropped before the release ...
        self.f.on_mpd_state("play")  # ... and after the reconnect a re-pause is queued.
        self.f.requeue_mpd_requests([ra.RELEASE_CAST])
        self.assertEqual(self.f.take_mpd_requests(), [ra.RELEASE_CAST, ra.PAUSE_PLAYER])

    def test_pause_without_an_owned_zone_does_not_poll(self):
        self.f.on_mpd_state("pause")
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_track_change_on_an_owned_zone_does_not_wake(self):
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("stop")
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing"] + ["ensure_playing(no wake)"] * 2)

    def test_user_resume_wakes(self):
        self.f.on_mpd_state("play")
        self.f.on_mpd_state("pause")
        self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing"])

    def test_taken_away_on_track_change_pauses_and_releases(self):
        self.f.on_mpd_state("play")
        self.ctl.decision = ra.STANDBY
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play")
        self.assertEqual(self.f.take_mpd_requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_requests_are_handed_out_once(self):
        self.f.on_mpd_state("play")
        self.ctl.verdict = ra.OTHER_SOURCE
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.assertEqual(self.f.take_mpd_requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertEqual(self.f.take_mpd_requests(), [])

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
        self.advance(ra.PLAY_RETRY_S - 1)
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.ctl.fail = None
        self.advance(1)
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing"])
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # now polling

    def test_play_retry_is_dropped_when_mpd_stops(self):
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play")
        self.f.on_mpd_state("stop")
        self.assertIsNone(self.f.seconds_until_deadline())

    def test_failed_poll_is_retried_and_not_taken_as_gone(self):
        self.f.on_mpd_state("play")
        self.ctl.fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.advance(5)
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.assertEqual(self.f.take_mpd_requests(), [])

    def test_device_supplied_error_text_is_logged_escaped(self):
        self.ctl.fail = ra.RaumfeldError("HTTP 500 Boom\n2026-01-01 INFO forged line")
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            self.f.on_mpd_state("play")
        self.assertNotIn("\n", logs.records[0].getMessage())
        self.assertIn("\\n", logs.records[0].getMessage())


if __name__ == "__main__":
    unittest.main()
