"""Pure logic: config, line-in detection, decisions, DIDL/SOAP building, Follower."""

import contextlib
import io
import logging
import time
import unittest
import xml.etree.ElementTree as ET
from contextlib import redirect_stderr
from unittest import mock

import raumfeld_adapter as ra
from fakes import LINE_IN_URI, ROOM_UDN, SPEAKER_UDN, STREAM_URL

@contextlib.contextmanager
def no_logs(test, name, level):
    """assertNoLogs for Python 3.9: a sentinel record must be the only one captured."""
    with test.assertLogs(name, level) as captured:
        yield
        logging.getLogger(name).log(level, "no_logs sentinel")
    sentinel = "%s:%s:no_logs sentinel" % (logging.getLevelName(level), name)
    test.assertEqual(captured.output, [sentinel])


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
        self.assertEqual(c.stuck_seconds, 15)
        self.assertEqual(c.start_gate_seconds, 15)

    def test_overrides(self):
        env = dict(REQUIRED, STREAM_MIME="audio/flac", STREAM_METADATA="0",
                   SPEAKER_POLL_SECONDS="2.5", MPD_HOST="mpd.invalid", MPD_PORT="6601",
                   LOG_LEVEL="debug", STUCK_SECONDS="20",
                   START_GATE_SECONDS="0")
        c = ra.Config.from_env(env)
        self.assertEqual(c.stream_mime, "audio/flac")
        self.assertFalse(c.stream_metadata)
        self.assertEqual(c.poll_seconds, 2.5)
        self.assertEqual((c.mpd_host, c.mpd_port), ("mpd.invalid", 6601))
        self.assertEqual(c.log_level, "DEBUG")
        self.assertEqual(c.stuck_seconds, 20)
        self.assertEqual(c.start_gate_seconds, 0)  # 0 turns the gate off

    def test_timing_upper_bounds_are_allowed(self):
        c = ra.Config.from_env(dict(REQUIRED, START_GATE_SECONDS="60", STUCK_SECONDS="300"))
        self.assertEqual((c.start_gate_seconds, c.stuck_seconds), (60, 300))

    def test_bound_is_named_in_the_error(self):
        with self.assertRaisesRegex(ra.ConfigError, "START_GATE_SECONDS must be 0-60"):
            ra.Config.from_env(dict(REQUIRED, START_GATE_SECONDS="90"))

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
                           ("STUCK_SECONDS", "0"), ("STUCK_SECONDS", "0.5"),
                           ("STUCK_SECONDS", "nan"), ("STUCK_SECONDS", "later"),
                           ("START_GATE_SECONDS", "-1"), ("START_GATE_SECONDS", "inf"),
                           ("START_GATE_SECONDS", "x"),
                           # A gate past 60 s only leaves the phone standing still; a
                           # renderer stuck for minutes is long gone.
                           ("START_GATE_SECONDS", "61"), ("STUCK_SECONDS", "301"),
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

    def test_main_passes_the_timing_settings_to_the_follower(self):
        with mock.patch.object(ra, "run") as run, mock.patch.object(ra.logging, "basicConfig"):
            ra.main(env=dict(REQUIRED, START_GATE_SECONDS="12", STUCK_SECONDS="25"))
        follower = run.call_args[0][0]
        self.assertEqual((follower.start_gate_seconds, follower.stuck_seconds), (12, 25))

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
        # TRANSITIONING is told apart, so a renderer stuck in it can be noticed.
        self.assertEqual(j("ACTIVE", zone, self.s("TRANSITIONING", STREAM_URL), STREAM_URL),
                         ra.TRANSITIONING)
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
        self.restart_fail = None
        self.plays = True
        self.plays_fail = None

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

    def plays_stream(self):
        self.calls.append("plays_stream")
        if self.plays_fail:
            raise self.plays_fail
        return self.plays

    def restart_renderer(self):
        self.calls.append("restart_renderer")
        if self.restart_fail:
            raise self.restart_fail


class FollowerTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=0)

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

    def test_unchanged_play_within_one_connection_is_no_new_start(self):
        self.f.on_mpd_state("play", "1")
        self.f.on_mpd_state("play", "1")  # e.g. a seek
        self.assertEqual(self.ctl.calls, ["ensure_playing"])

    def test_unchanged_play_after_a_reconnect_restarts_an_owned_zone(self):
        # MPD restarted and restored play with the same songid (ids restart at 0): the
        # stream died with MPD, so the renderer is told to play again (no wake).
        self.f.on_mpd_state("play", "1")
        self.f.on_mpd_connected()
        self.f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing(no wake)"])

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


class StuckRendererTest(unittest.TestCase):
    """A zone renderer that stays TRANSITIONING on our stream while MPD plays has dropped
    the stream; Stop then Play brings it back, a plain Play does not."""

    STUCK_S = 15

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             stuck_seconds=self.STUCK_S, start_gate_seconds=0)
        self.f.on_mpd_state("play")
        self.ctl.calls.clear()

    def poll(self, times=1):
        for _ in range(times):
            self.clock.now += 5
            self.f.tick()

    def restarts(self):
        return self.ctl.calls.count("restart_renderer")

    def test_brief_transitioning_does_not_restart(self):
        self.ctl.verdict = ra.TRANSITIONING
        self.poll(3)  # first seen, +5 s, +10 s: still within the bound
        self.ctl.verdict = ra.OURS
        self.poll(3)
        self.assertEqual(self.restarts(), 0)
        self.assertEqual(self.f.take_mpd_requests(), [])

    def test_stuck_beyond_the_bound_restarts_and_is_not_taken_away(self):
        self.ctl.verdict = ra.TRANSITIONING
        self.poll(3)
        self.assertEqual(self.restarts(), 0)
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            self.poll()  # 15 s after it was first seen
        self.assertEqual(self.restarts(), 1)
        self.assertTrue(any("TRANSITIONING" in m for m in logs.output), logs.output)
        self.assertEqual(self.f.take_mpd_requests(), [])  # no pause, no release
        self.assertFalse(self.f.yielded)
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # still watching

    def test_restarts_are_bounded_with_backoff(self):
        self.ctl.verdict = ra.TRANSITIONING
        times = []
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            for _ in range(100):  # 500 s of a renderer that never recovers
                before = self.restarts()
                self.poll()
                if self.restarts() > before:
                    times.append(self.clock.now)
        self.assertEqual(len(times), ra.STUCK_MAX_RESTARTS)
        gaps = [b - a for a, b in zip(times, times[1:])]
        self.assertEqual(gaps, sorted(gaps))
        self.assertGreater(gaps[-1], gaps[0])
        self.assertTrue(any("giving up" in m for m in logs.output), logs.output)
        self.assertEqual(sum("giving up" in m for m in logs.output), 1)
        self.assertEqual(self.f.take_mpd_requests(), [])

    def test_playing_again_resets_the_episode(self):
        self.ctl.verdict = ra.TRANSITIONING
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.poll(4)
        self.assertEqual(self.restarts(), 1)
        self.ctl.verdict = ra.OURS
        self.poll()
        self.ctl.verdict = ra.TRANSITIONING
        self.poll(3)
        self.assertEqual(self.restarts(), 1)  # a new episode starts its own clock
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.poll()
        self.assertEqual(self.restarts(), 2)

    def test_our_own_stop_read_as_not_playing_does_not_end_the_episode(self):
        self.ctl.verdict = ra.TRANSITIONING
        times = []
        restarted = False
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            for _ in range(100):
                # The poll right after each restart reads STOPPED: our own Stop.
                self.ctl.verdict = ra.NOT_PLAYING if restarted else ra.TRANSITIONING
                before = self.restarts()
                self.poll()
                restarted = self.restarts() > before
                if restarted:
                    times.append(self.clock.now)
        self.assertEqual(len(times), ra.STUCK_MAX_RESTARTS)
        # The backoff continues across the stopped poll: 2 x 15 s, then 4 x 15 s.
        self.assertEqual([b - a for a, b in zip(times, times[1:])],
                         [2 * self.STUCK_S, 4 * self.STUCK_S])
        self.assertEqual(self.f.take_mpd_requests(), [])

    def test_transitioning_while_mpd_is_paused_is_left_alone(self):
        self.f.on_mpd_state("pause")
        self.ctl.verdict = ra.TRANSITIONING
        self.poll(10)
        self.assertEqual(self.restarts(), 0)

    def test_transitioning_does_not_count_as_stopped(self):
        self.ctl.verdict = ra.NOT_PLAYING
        self.poll(ra.STOPPED_GRACE_POLLS)
        self.ctl.verdict = ra.TRANSITIONING
        self.poll()
        self.ctl.verdict = ra.NOT_PLAYING
        self.poll(ra.STOPPED_GRACE_POLLS)
        self.assertEqual(self.f.take_mpd_requests(), [])

    def test_failed_restart_is_logged_and_counts_as_an_attempt(self):
        self.ctl.verdict = ra.TRANSITIONING
        self.ctl.restart_fail = ra.RaumfeldError("took longer than the request deadline")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.poll(4)
        self.assertEqual(self.restarts(), 1)
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.assertEqual(self.f.take_mpd_requests(), [])


class StartGateTest(unittest.TestCase):
    """The speaker needs seconds to wake and buffer: at a takeover the adapter holds MPD at
    the start of the song (pause 1) until the renderer plays our stream, then resumes it."""

    GATE_S = 15

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.plays = False
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             start_gate_seconds=self.GATE_S)

    def advance(self, seconds):
        self.clock.now += seconds
        self.f.tick()

    def requests(self):
        return self.f.take_mpd_requests()

    def held(self, song="1"):
        """A cast starts: MPD is paused before the room is touched, then taken over."""
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", song)
        self.assertEqual(self.ctl.calls, [])  # nothing woken before MPD holds the song
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.f.on_mpd_state("pause", song)  # run() reports our pause 1
        self.assertEqual(self.ctl.calls, ["ensure_playing"])

    def released(self, song="1"):
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.f.on_mpd_state("play", song)  # run() reports our pause 0

    def test_takeover_holds_mpd_until_the_renderer_plays(self):
        self.held()
        self.assertEqual(self.f.seconds_until_deadline(), ra.GATE_POLL_S)
        self.advance(ra.GATE_POLL_S)
        self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [])  # still connecting and buffering
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.f.on_mpd_state("play", "1")  # the idle event of our own pause 0
        self.assertEqual(self.ctl.calls, ["ensure_playing"] + ["plays_stream"] * 3)
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # watched as usual from now on
        self.assertEqual(self.requests(), [])

    def test_own_pause_and_resume_are_not_followed_to_the_speaker(self):
        self.held()
        self.f.on_mpd_state("pause", "1")  # the idle event of our own pause 1
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.assertEqual(self.ctl.calls, ["ensure_playing", "plays_stream"])  # no wake again
        self.assertFalse(self.f.yielded)
        self.assertEqual(self.requests(), [])  # no release

    def test_renderer_never_playing_unpauses_after_the_max_gate_time(self):
        self.held()
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            while self.clock.now < 1000 + self.GATE_S + 1:
                self.advance(ra.GATE_POLL_S)
                if self.f._requests:
                    break
        self.assertAlmostEqual(self.clock.now, 1000 + self.GATE_S, delta=ra.GATE_POLL_S)
        self.assertTrue(any("anyway" in m for m in logs.output), logs.output)
        self.released()
        self.assertEqual(self.f.seconds_until_deadline(), 5)

    def test_failing_renderer_checks_keep_the_gate_bounded(self):
        self.held()
        self.ctl.plays_fail = OSError("connection refused")  # the speaker is still booting
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            for _ in range(int(self.GATE_S / ra.GATE_POLL_S) + 1):
                self.advance(ra.GATE_POLL_S)
        self.released()

    def test_failed_takeover_unpauses_at_once_and_retries_later(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.released()
        self.f.on_mpd_state("play", "1")  # the idle event of our own pause 0 is no new cast
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.f.seconds_until_deadline(), ra.PLAY_RETRY_S)
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.ctl.fail = None
        self.advance(ra.PLAY_RETRY_S)  # the retry takes over without holding MPD again
        self.assertEqual(self.ctl.calls, ["ensure_playing", "ensure_playing"])
        self.assertEqual(self.requests(), [])

    def test_stop_from_the_phone_during_the_gate_is_not_overridden(self):
        self.held()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("stop", None)
        self.ctl.plays = True
        for _ in range(4):
            self.advance(5)
        self.assertEqual(self.requests(), [])  # never pause 0
        self.assertNotIn("plays_stream", self.ctl.calls)

    def test_song_change_during_the_gate_is_not_overridden_and_does_not_wake(self):
        for state in ("play", "pause"):
            with self.subTest(state=state):
                self.setUp()
                self.held()
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state(state, "2")  # the phone skipped to the next song
                self.ctl.plays = True
                for _ in range(4):
                    self.advance(5)
                self.assertEqual(self.requests(), [])
                self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)

    def test_song_change_while_our_resume_is_on_its_way_ends_the_gate(self):
        self.held()
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "2")  # the phone skipped right as pause 0 landed
        # MPD-supplied values are logged with %r.
        self.assertTrue(any("MPD changed during the start gate ('play', song '2')" in m
                            for m in logs.output), logs.output)
        self.assertIsNone(self.f._gate)
        self.assertEqual(self.ctl.calls[-1], "ensure_playing(no wake)")  # a song change
        self.assertEqual(self.requests(), [])

    def test_stop_before_our_pause_lands_drops_the_pause(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        # The connection dropped before pause 1 went out; after the reconnect MPD is stopped.
        self.f.requeue_mpd_requests(self.requests())
        self.f.on_mpd_state("stop", None)
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.ctl.calls, [])

    def test_play_reported_before_our_pause_lands_keeps_the_gate(self):
        # The connection dropped before pause 1 went out; the reconnect's first status still
        # plays, the same song or one the phone skipped to meanwhile. Our pause, still to go
        # out, holds whichever song MPD plays.
        for song in ("1", "2"):
            with self.subTest(song=song):
                self.setUp()
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state("play", "1")
                self.f.requeue_mpd_requests(self.requests())
                self.f.on_mpd_connected()
                gate, deadline = self.f._gate, self.f.deadline
                self.clock.now += 3  # a gate opened now would have a later deadline
                with no_logs(self, "raumfeld_adapter", logging.INFO):
                    self.f.on_mpd_state("play", song)
                self.assertIs(self.f._gate, gate)
                self.assertEqual(self.f.deadline, deadline)
                self.assertEqual(self.ctl.calls, [])
                self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
                self.f.on_mpd_state("pause", song)  # run() reports our pause 1
                self.assertEqual(self.ctl.calls, ["ensure_playing"])
                self.assertEqual(self.f._gate.song, song)

    def test_unowned_play_after_a_reconnect_is_gated_once(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.released()  # the takeover failed; MPD plays, the retry is pending
        self.f.on_mpd_connected()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")  # unchanged, but on a new connection
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.f.on_mpd_state("play", "1")  # unchanged on the same connection: no new gate
        self.assertEqual(self.requests(), [])

    def test_song_change_in_an_owned_session_is_not_gated(self):
        self.held()
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.f.on_mpd_state("stop", None)
        self.f.on_mpd_state("play", "2")
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.ctl.calls[-1], "ensure_playing(no wake)")

    def test_resume_in_an_owned_session_is_not_gated(self):
        self.held()
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.f.on_mpd_state("pause", "1")  # the user pauses ...
        self.f.on_mpd_state("play", "1")  # ... and resumes: the speaker kept the stream
        self.assertEqual(self.requests(), [])

    def test_resume_after_a_yield_is_gated(self):
        self.held()
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.ctl.verdict = ra.STANDBY
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.f.on_mpd_state("pause", "1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")  # the user resumes from the phone
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])

    def test_unconfirmed_pause_takes_over_without_the_gate_at_the_deadline(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.f.seconds_until_deadline(), self.GATE_S)
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.advance(self.GATE_S)  # pause 1 never confirmed (MPD unreachable meanwhile)
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertEqual(self.requests(), [])  # the stale pause 1 is dropped
        self.assertEqual(self.f.seconds_until_deadline(), ra.PLAY_RETRY_S)

    def test_gate_is_on_by_default_like_the_config(self):
        self.assertEqual(ra.Follower(self.ctl).start_gate_seconds, ra.START_GATE_S)
        self.assertEqual(ra.Config.from_env(REQUIRED).start_gate_seconds, ra.START_GATE_S)

    def test_gate_off_takes_over_at_once(self):
        f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=0)
        f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertEqual(f.take_mpd_requests(), [])


if __name__ == "__main__":
    unittest.main()
