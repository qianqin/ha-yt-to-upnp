"""Pure logic: config, line-in detection, decisions, DIDL/SOAP building, Follower."""

import contextlib
import io
import os
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


def no_network():
    """main() looks for the room before it follows MPD; tests never reach the LAN."""
    return mock.patch.object(ra.Controller, "warm_up")


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

    def test_raumfeld_host_is_optional(self):
        self.assertIsNone(ra.Config.from_env(REQUIRED).raumfeld_host)
        self.assertIsNone(ra.Config.from_env(dict(REQUIRED, RAUMFELD_HOST=" ")).raumfeld_host)

    def test_raumfeld_host_takes_an_ipv4_address_or_a_dns_name(self):
        for host in ("192.0.2.10", "raumfeld-host", "raumfeld-host.local", "host.example.org"):
            with self.subTest(host=host):
                c = ra.Config.from_env(dict(REQUIRED, RAUMFELD_HOST=" %s " % host))
                self.assertEqual(c.raumfeld_host, host)

    def test_with_raumfeld_host_the_speaker_udn_is_not_needed(self):
        env = {"RAUMFELD_ROOM_UDN": ROOM_UDN, "STREAM_URL": STREAM_URL,
               "RAUMFELD_HOST": "raumfeld-host.local"}
        self.assertIsNone(ra.Config.from_env(env).speaker_udn)
        with self.assertRaisesRegex(ra.ConfigError, "RAUMFELD_SPEAKER_UDN"):
            ra.Config.from_env(dict(env, RAUMFELD_SPEAKER_UDN="speaker"))  # still checked
        del env["RAUMFELD_HOST"]
        with self.assertRaisesRegex(ra.ConfigError, "RAUMFELD_SPEAKER_UDN"):
            ra.Config.from_env(env)

    def test_invalid_raumfeld_host(self):
        for host in ("http://192.0.2.10", "192.0.2.10:47365", "host/path", "a b", "-host",
                     "host-", "host..local", "999.0.2.10", "192.0.2", "::1", "[::1]",
                     "host\r\nX: 1", "a" * 64 + ".local", ("a" * 60 + ".") * 5 + "local"):
            with self.subTest(host=host), \
                    self.assertRaisesRegex(ra.ConfigError, "RAUMFELD_HOST"):
                ra.Config.from_env(dict(REQUIRED, RAUMFELD_HOST=host))

    def test_adapter_state_dir_is_optional_and_must_be_absolute(self):
        self.assertIsNone(ra.Config.from_env(REQUIRED).state_dir)
        self.assertEqual(ra.Config.from_env(dict(REQUIRED, ADAPTER_STATE_DIR=" /data ")).state_dir,
                         "/data")
        for value in ("data", "./data", "/data\nx"):
            with self.subTest(value=value), \
                    self.assertRaisesRegex(ra.ConfigError, "ADAPTER_STATE_DIR"):
                ra.Config.from_env(dict(REQUIRED, ADAPTER_STATE_DIR=value))

    def test_main_keeps_the_state_file_in_the_state_dir(self):
        with mock.patch.object(ra, "run") as run, no_network(), \
                mock.patch.object(ra.logging, "basicConfig"):
            ra.main(env=dict(REQUIRED, ADAPTER_STATE_DIR="/data"))
            ra.main(env=dict(REQUIRED))
        with_dir, without = (c[0][0].controller.state for c in run.call_args_list)
        self.assertEqual(with_dir.path, os.path.join("/data", ra.STATE_FILE_NAME))
        self.assertIsNone(without)

    def test_idle_minutes_is_no_longer_a_setting(self):
        c = ra.Config.from_env(dict(REQUIRED, IDLE_MINUTES="soon"))  # ignored, not validated
        self.assertFalse(hasattr(c, "idle_seconds"))

    def test_main_warns_that_idle_minutes_is_ignored(self):
        with mock.patch.object(ra, "run") as run, no_network(), \
                mock.patch.object(ra.logging, "basicConfig"), \
                self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            code = ra.main(env=dict(REQUIRED, IDLE_MINUTES="5"))
        self.assertEqual(code, 0)
        run.assert_called_once()
        self.assertTrue(any(r.levelname == "WARNING" and "IDLE_MINUTES" in r.getMessage()
                            for r in logs.records), logs.output)

    def test_main_passes_the_timing_settings_to_the_follower(self):
        with mock.patch.object(ra, "run") as run, no_network(), \
                mock.patch.object(ra.logging, "basicConfig"):
            ra.main(env=dict(REQUIRED, START_GATE_SECONDS="12", STUCK_SECONDS="25"))
        follower = run.call_args[0][0]
        self.assertEqual((follower.start_gate_seconds, follower.stuck_seconds), (12, 25))

    def test_main_does_not_warn_without_idle_minutes(self):
        with mock.patch.object(ra, "run"), no_network(), \
                mock.patch.object(ra.logging, "basicConfig"), \
                self.assertLogs("raumfeld_adapter", "INFO") as logs:
            ra.main(env=dict(REQUIRED))
        self.assertFalse(any("IDLE_MINUTES" in m for m in logs.output), logs.output)

    def test_main_finds_the_room_before_following_mpd(self):
        order = []
        with mock.patch.object(ra, "run", lambda *a, **k: order.append("run")), \
                mock.patch.object(ra.Controller, "warm_up", lambda self: order.append("warm_up")), \
                mock.patch.object(ra.logging, "basicConfig"):
            ra.main(env=dict(REQUIRED))
        self.assertEqual(order, ["warm_up", "run"])

    def test_main_locates_by_ssdp_or_by_the_configured_host(self):
        for env, kind in ((REQUIRED, ra.SsdpLocator),
                          (dict(REQUIRED, RAUMFELD_HOST="raumfeld-host.local"), ra.HostLocator)):
            with self.subTest(kind=kind.__name__), mock.patch.object(ra, "run") as run, \
                    no_network(), mock.patch.object(ra.logging, "basicConfig"):
                ra.main(env=env)
            self.assertIsInstance(run.call_args[0][0].controller.locator, kind)

    def test_main_exits_nonzero_with_clear_message(self):
        err = io.StringIO()
        with redirect_stderr(err):
            code = ra.main(env={})
        self.assertNotEqual(code, 0)
        self.assertIn("STREAM_URL", err.getvalue())
        self.assertIn("RAUMFELD_ROOM_UDN", err.getvalue())


class MaxVolumeConfigTest(unittest.TestCase):
    def test_max_volume_defaults_to_50(self):
        self.assertEqual(ra.Config.from_env(REQUIRED).max_volume, 50)

    def test_max_volume_is_a_whole_number_from_1_to_100(self):
        for value in ("1", "100", " 42 "):
            self.assertEqual(ra.Config.from_env(dict(REQUIRED, MAX_VOLUME=value)).max_volume,
                             int(value))
        for value in ("0", "101", "-5", "50.5", "loud", ""):
            with self.subTest(value=value), self.assertRaisesRegex(ra.ConfigError, "MAX_VOLUME"):
                ra.Config.from_env(dict(REQUIRED, MAX_VOLUME=value))

    def test_main_holds_mpd_until_playing_except_for_flac(self):
        for mime, hold in (("audio/flac", False), ("AUDIO/FLAC", False), ("audio/mpeg", True)):
            with self.subTest(mime=mime), no_network(), mock.patch.object(ra, "run") as run, \
                    mock.patch("logging.basicConfig"):
                ra.main(dict(REQUIRED, STREAM_MIME=mime))
                self.assertEqual(run.call_args[0][0].hold_until_playing, hold)

    def test_main_passes_max_volume_to_the_follower(self):
        env = dict(REQUIRED, MAX_VOLUME="35")
        with no_network(), mock.patch.object(ra, "run") as run, mock.patch("logging.basicConfig"):
            ra.main(env)
        self.assertEqual(run.call_args[0][0].max_volume, 35)


class VolumeScaleTest(unittest.TestCase):
    """Phone 0-100 % is speaker 0..max_volume, linear, rounded half up."""

    def test_phone_to_speaker(self):
        self.assertEqual(ra.speaker_volume(100, 50), 50)
        self.assertEqual(ra.speaker_volume(0, 50), 0)
        self.assertEqual(ra.speaker_volume(51, 50), 26)  # 25.5
        self.assertEqual(ra.speaker_volume(33, 30), 10)  # 9.9
        self.assertEqual(ra.speaker_volume(1, 1), 0)
        self.assertEqual(ra.speaker_volume(50, 1), 1)

    def test_never_above_max_volume_nor_below_zero(self):
        for max_volume in range(1, 101):
            for phone in (-10, 0, 1, 50, 99, 100, 150):
                with self.subTest(max_volume=max_volume, phone=phone):
                    self.assertTrue(0 <= ra.speaker_volume(phone, max_volume) <= max_volume)

    def test_speaker_to_phone_is_clamped_to_100(self):
        self.assertEqual(ra.phone_volume(25, 50), 50)
        self.assertEqual(ra.phone_volume(50, 50), 100)
        self.assertEqual(ra.phone_volume(80, 50), 100)  # louder than the app's maximum
        self.assertEqual(ra.phone_volume(0, 50), 0)
        self.assertEqual(ra.phone_volume(1, 3), 33)

    def test_the_round_trip_keeps_the_speaker_volume(self):
        # The volume the adapter gives MPD maps back onto the speaker's own: no drift.
        for max_volume in range(1, 101):
            for speaker in range(0, max_volume + 1):
                self.assertEqual(
                    ra.speaker_volume(ra.phone_volume(speaker, max_volume), max_volume), speaker)


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

    def test_another_source_plays(self):
        p = ra.plays_another_source
        self.assertFalse(p(None, STREAM_URL))
        for state in ("PLAYING", "TRANSITIONING"):
            for uri in (LINE_IN_URI, "http://other.invalid/x"):
                self.assertTrue(p(self.s(state, uri), STREAM_URL))
            self.assertFalse(p(self.s(state, STREAM_URL), STREAM_URL))
            self.assertFalse(p(self.s(state, ""), STREAM_URL))  # nothing loaded
        self.assertFalse(p(self.s("STOPPED", LINE_IN_URI), STREAM_URL))

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
        self.pause_fail = None
        self.play_fail = None
        # RenderingControl, recorded apart: most tests are about the transport.
        self.volume_calls = []
        self.speaker_volume = 30
        self.volume_fail = None
        self.rendering = True  # the renderer has RenderingControl
        self.position_calls = []
        self.heard = None  # GetPositionInfo RelTime in seconds; None: unknown
        self.position_fail = None
        self.plays = True
        self.plays_fail = None
        self.renderer = "STOPPED"  # what renderer_state reports while not playing
        self.clock = None
        self.takeover_s = 0
        self.renderer_s = 0
        self.retake_decision = None  # what a re-takeover finds, if not self.decision
        self.takeover_times = []  # clock at the start of each takeover
        self.last_takeover = ra.TakeoverStats(wake_s=0.0, lookups=0)

    def ensure_playing(self, wake=True, retake=False):
        self.calls.append("ensure_playing(retake)" if retake else
                          "ensure_playing" if wake else "ensure_playing(no wake)")
        if self.clock is not None:
            self.takeover_times.append(self.clock.now)
        if self.takeover_s:
            self.clock.now += self.takeover_s  # a slow speaker
        if self.fail:
            raise self.fail
        if retake and self.retake_decision:
            return self.retake_decision
        return self.decision

    def check(self):
        self.calls.append("check")
        if self.fail:
            raise self.fail
        return self.verdict

    def renderer_state(self):
        self.calls.append("renderer_state")
        if self.renderer_s:
            self.clock.now += self.renderer_s  # a slow renderer
        if self.plays_fail:
            raise self.plays_fail
        return "PLAYING" if self.plays else self.renderer

    def restart_renderer(self):
        self.calls.append("restart_renderer")
        if self.restart_fail:
            raise self.restart_fail

    def pause_renderer(self):
        self.calls.append("pause_renderer")
        if self.pause_fail:
            raise self.pause_fail
        self.plays, self.renderer = False, "PAUSED_PLAYBACK"

    def play_renderer(self):
        self.calls.append("play_renderer")
        if self.play_fail:
            raise self.play_fail

    def position(self):
        self.position_calls.append(self.clock.now if self.clock else None)
        if self.position_fail:
            raise self.position_fail
        return self.heard

    def has_volume(self):
        return self.rendering

    def get_volume(self):
        self.volume_calls.append("get_volume")
        if self.volume_fail:
            raise self.volume_fail
        return self.speaker_volume

    def set_volume(self, volume):
        self.volume_calls.append("set_volume %d" % volume)
        if self.volume_fail:
            raise self.volume_fail
        self.speaker_volume = volume


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
        self.ctl.plays = False  # not ours to pause (a followed pause: PauseFollowTest)
        self.f.on_mpd_state("pause")
        self.advance(5)
        self.f.on_mpd_state("stop")
        self.advance(5)
        self.assertEqual(self.ctl.calls, ["ensure_playing", "renderer_state", "check", "check"])

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
        self.ctl.plays = False  # the renderer stopped meanwhile: nothing of ours to pause
        self.f.on_mpd_state("pause")
        self.f.on_mpd_state("play")
        self.assertEqual(self.ctl.calls, ["ensure_playing", "renderer_state", "ensure_playing"])

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


# More renderer looks than the longest gate (START_GATE_MAX_S) can take.
GATE_LOOKS = int(ra.START_GATE_MAX_S / ra.GATE_POLL_S) + 10


class StartGateTest(unittest.TestCase):
    """The speaker needs seconds to wake and buffer: at a takeover the adapter holds MPD at
    the start of the song (pause 1) until the renderer plays our stream, then resumes it."""

    GATE_S = 15

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.plays = False
        self.ctl.clock = self.clock
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

    def gate_runs_out(self, level="INFO"):
        """Tick the held gate until it asks for pause 0; the logs of the way there."""
        with self.assertLogs("raumfeld_adapter", level) as logs:
            for _ in range(GATE_LOOKS):
                self.advance(ra.GATE_POLL_S)
                if self.f._requests:
                    break
        return logs.output

    def timed_out(self):
        """A cast whose renderer never plays during the gate: MPD is resumed anyway."""
        self.held()
        self.gate_runs_out()
        self.released()
        self.ctl.calls.clear()

    def poll(self, times=1):
        for _ in range(times):
            self.advance(5)

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
        self.assertEqual(self.ctl.calls, ["ensure_playing"] + ["renderer_state"] * 3)
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # watched as usual from now on
        self.assertEqual(self.requests(), [])

    def test_the_renderer_is_asked_every_quarter_second_during_the_gate(self):
        # Every look costs two SOAP requests: at most 8 per second, for at most 60 s.
        self.held()
        self.assertEqual(self.f.seconds_until_deadline(), 0.25)
        self.advance(0.25)
        self.assertEqual(self.ctl.calls, ["ensure_playing", "renderer_state"])
        self.assertEqual(self.f.seconds_until_deadline(), 0.25)

    def test_own_pause_and_resume_are_not_followed_to_the_speaker(self):
        self.held()
        self.f.on_mpd_state("pause", "1")  # the idle event of our own pause 1
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.assertEqual(self.ctl.calls, ["ensure_playing", "renderer_state"])  # no wake again
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

    # -- re-takeover during the gate (a speaker still shutting down drops the first one)

    def test_renderer_not_playing_is_taken_over_again_after_the_retake_delay(self):
        self.held()
        for _ in range(int(ra.GATE_RETAKE_S / ra.GATE_POLL_S) - 1):
            self.advance(ra.GATE_POLL_S)
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.ctl.calls[-1], "ensure_playing(retake)")
        self.assertTrue(any("'STOPPED'" in m and "again" in m for m in logs.output), logs.output)
        self.assertEqual(self.requests(), [])  # MPD is still held
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 1)

    def test_retakeovers_are_bounded_and_the_gate_still_ends(self):
        self.ctl.renderer = "NO_MEDIA_PRESENT"  # the takeover was dropped
        self.held()
        self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        # A takeover that returns at once needs no more time than the configured gate.
        self.assertEqual(self.clock.now, 1000 + self.GATE_S)
        self.released()

    def test_a_slow_takeover_extends_the_gate_so_each_retake_has_time_to_play(self):
        self.ctl.takeover_s = 3  # as in the live incident
        self.held()  # the first takeover returns at +3 s
        logs = self.gate_runs_out()
        # Retakes at +8 s (back at +11 s) and +16 s (back at +19 s); the second one gets
        # GATE_RETAKE_S to play, past the configured 15 s.
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 2)
        self.assertEqual(self.clock.now, 1019 + ra.GATE_RETAKE_S)
        self.assertTrue(any("after 24.0 s" in m for m in logs), logs)
        self.released()

    def test_a_takeover_slower_than_the_gate_still_gets_time_to_play(self):
        self.ctl.takeover_s = 12  # e.g. the wake wait plus slow speaker requests
        self.held()  # back at +12 s: the gate now ends at +17 s, not +15 s
        self.ctl.takeover_s = 0
        self.gate_runs_out()
        # Retakes at +17 s and +22 s, each given GATE_RETAKE_S.
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 2)
        self.assertEqual(self.clock.now, 1000 + 22 + ra.GATE_RETAKE_S)
        self.released()

    def test_a_short_gate_still_gives_each_retake_its_time(self):
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=3)
        self.held()
        self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        self.assertEqual(self.clock.now, 1000 + (ra.GATE_MAX_RETAKES + 1) * ra.GATE_RETAKE_S)
        self.released()

    def test_the_gate_never_holds_mpd_beyond_the_configured_maximum(self):
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             start_gate_seconds=ra.START_GATE_MAX_S)
        self.ctl.takeover_s = 27
        self.held()  # back at +27 s; the retake at +32 s is back at +59 s
        self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 1)
        self.assertEqual(self.clock.now, 1000 + ra.START_GATE_MAX_S)
        self.released()

    def test_failed_retakes_are_logged_and_keep_the_gate_bounded(self):
        self.held()
        self.ctl.fail = OSError("speaker unreachable")
        logs = self.gate_runs_out("WARNING")
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        self.assertTrue(any("speaker unreachable" in m for m in logs), logs)
        self.assertEqual(self.clock.now, 1000 + self.GATE_S)
        self.released()

    def max_gate(self):
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             start_gate_seconds=ra.START_GATE_MAX_S)
        return 1000 + ra.START_GATE_MAX_S

    def test_no_retake_starts_that_could_not_get_its_time_before_the_maximum(self):
        cap = self.max_gate()
        # Back at +41 s: a retake due at +46 s, with the wake wait and its own time to
        # play, would end past the maximum.
        self.ctl.takeover_s = cap - 1000 - ra.WAKE_WAIT_S - ra.GATE_RETAKE_S - ra.GATE_RETAKE_S + 1
        self.held()
        self.gate_runs_out()
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.clock.now, cap)
        self.released()

    def test_a_retake_that_just_gets_its_time_before_the_maximum_is_still_sent(self):
        cap = self.max_gate()
        self.ctl.takeover_s = cap - 1000 - ra.WAKE_WAIT_S - ra.GATE_RETAKE_S - ra.GATE_RETAKE_S
        self.held()
        self.ctl.takeover_s = 0
        self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 1)
        self.released()

    def test_a_slow_renderer_check_late_in_the_gate_starts_no_retake(self):
        cap = self.max_gate()
        self.ctl.takeover_s = 45
        self.ctl.renderer_s = 4  # each look at the renderer takes 4 s
        self.held()
        self.gate_runs_out()
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertTrue(all(t < cap for t in self.ctl.takeover_times), self.ctl.takeover_times)
        self.released()

    def assert_yielded_from_the_gate(self):
        """MPD stays paused (no pause 0, no second pause 1), the cast is released, the gate
        is over and the room left alone."""
        self.assertEqual(self.requests(), [ra.RELEASE_CAST])
        self.assertTrue(self.f.yielded)
        self.assertFalse(self.f.owned)
        self.assertIsNone(self.f._gate)
        self.assertIsNone(self.f.seconds_until_deadline())
        self.f.on_mpd_state("stop", None)  # the receiver ends the session
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.f.yielded)

    def test_renderer_playing_a_new_source_is_respected_straight_from_the_gate(self):
        for seen in ("PLAYING (another source)", "TRANSITIONING (another source)"):
            with self.subTest(seen=seen):
                self.setUp()
                self.ctl.renderer = seen  # someone started another source during the gate
                self.held()
                logs = self.gate_runs_out("INFO")
                self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
                self.assertNotIn("restart_renderer", self.ctl.calls)
                # Not taken for our stream playing.
                self.assertFalse(any("speaker plays" in m for m in logs), logs)
                self.assertTrue(any(seen in m for m in logs), logs)
                self.assertEqual(self.clock.now, 1000 + ra.GATE_POLL_S)  # at the first look
                self.assert_yielded_from_the_gate()

    def test_renderer_back_on_the_source_before_the_takeover_is_taken_over_again(self):
        # e.g. line-in the cast took over: the speaker dropped the takeover.
        self.ctl.renderer = "PLAYING (source before the takeover)"
        self.held()
        logs = self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        self.assertTrue(any("taking over again" in m for m in logs), logs)
        self.released()  # the gate ran out as usual: pause 0
        self.assertFalse(self.f.yielded)

    def test_retakes_of_the_source_before_the_takeover_keep_the_60_s_bound(self):
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             start_gate_seconds=ra.START_GATE_MAX_S)
        self.ctl.renderer = "PLAYING (source before the takeover)"
        self.ctl.takeover_s = 41
        self.held()
        self.gate_runs_out()
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.clock.now, 1000 + ra.START_GATE_MAX_S)
        self.released()

    def test_renderer_stopped_on_another_uri_is_taken_over_again(self):
        self.ctl.renderer = "STOPPED (another source)"  # our URI never got loaded
        self.held()
        self.gate_runs_out()
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        self.released()

    def test_renderer_with_nothing_loaded_is_taken_over_again_whatever_its_state(self):
        for seen in ("PLAYING (nothing loaded)", "TRANSITIONING (nothing loaded)"):
            with self.subTest(seen=seen):
                self.setUp()
                self.ctl.renderer = seen  # no source anyone chose
                self.held()
                self.gate_runs_out()
                self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"),
                                 ra.GATE_MAX_RETAKES)
                self.released()
                self.assertFalse(self.f.yielded)

    def test_a_retake_finding_a_new_source_yields_straight_from_the_gate(self):
        self.ctl.retake_decision = ra.OTHER_SOURCE
        self.held()
        logs = self.gate_runs_out("INFO")
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 1)
        self.assertEqual(self.clock.now, 1000 + ra.GATE_RETAKE_S)  # right after the retake
        self.assertTrue(any("another source" in m for m in logs), logs)
        self.assert_yielded_from_the_gate()

    def test_renderer_transitioning_on_our_stream_is_restarted_not_taken_over_again(self):
        self.ctl.renderer = "TRANSITIONING"  # connecting and buffering -- or hanging
        self.held()
        for _ in range(int(2 * ra.GATE_RETAKE_S / ra.GATE_POLL_S) - 1):
            self.advance(ra.GATE_POLL_S)
        self.assertNotIn("restart_renderer", self.ctl.calls)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.ctl.calls[-1], "restart_renderer")
        self.assertTrue(any("TRANSITIONING" in m for m in logs.output), logs.output)
        self.assertEqual(self.requests(), [])  # MPD is still held
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.ctl.calls.count("restart_renderer"), 1)

    def test_renderer_still_transitioning_at_gate_end_is_left_to_the_stuck_handling(self):
        self.ctl.renderer = "TRANSITIONING"
        self.held()
        logs = self.gate_runs_out("WARNING")
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.ctl.calls.count("restart_renderer"), 1)  # once per gate
        self.assertTrue(any("renderer 'TRANSITIONING'" in m for m in logs), logs)
        self.released()
        self.ctl.calls.clear()
        self.ctl.verdict = ra.TRANSITIONING
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            for _ in range(int(ra.STUCK_S / 5) + 1):
                self.poll()
        self.assertEqual(self.ctl.calls.count("restart_renderer"), 1)  # T21 takes over
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.f.yielded)

    def test_a_transitioning_renderer_is_restarted_once_however_long_the_gate(self):
        self.max_gate()
        self.ctl.renderer = "TRANSITIONING"
        self.held()
        self.gate_runs_out("WARNING")
        self.assertEqual(self.ctl.calls.count("restart_renderer"), 1)
        self.assertEqual(self.clock.now, 1000 + ra.START_GATE_MAX_S)
        self.released()

    def test_a_slow_takeover_of_a_transitioning_renderer_still_gets_its_restart(self):
        # A speaker woken from standby: the takeover takes longer than GATE_RETAKE_S.
        for takeover_s in (ra.GATE_RETAKE_S + 3, ra.WAKE_WAIT_S + 2):
            with self.subTest(takeover_s=takeover_s):
                self.setUp()
                self.ctl.renderer = "TRANSITIONING"
                self.ctl.takeover_s = takeover_s
                self.held()
                self.ctl.takeover_s = 0
                logs = self.gate_runs_out("WARNING")
                self.assertEqual(self.ctl.calls.count("restart_renderer"), 1)
                self.assertNotIn("ensure_playing(retake)", self.ctl.calls)
                # Restarted 2 * GATE_RETAKE_S after the takeover returned; then its own time.
                self.assertEqual(self.clock.now,
                                 1000 + takeover_s + 2 * ra.GATE_RETAKE_S + ra.GATE_RETAKE_S)
                self.assertTrue(any("renderer 'TRANSITIONING'" in m for m in logs), logs)
                self.released()

    def test_waiting_for_the_restart_keeps_the_60_s_bound(self):
        # Back at +52 s: the restart would be due at +62 s, past the maximum.
        self.ctl.renderer = "TRANSITIONING"
        self.ctl.takeover_s = ra.START_GATE_MAX_S - 8
        self.held()
        self.ctl.takeover_s = 0
        self.gate_runs_out("WARNING")
        self.assertNotIn("restart_renderer", self.ctl.calls)
        self.assertEqual(self.clock.now, 1000 + ra.START_GATE_MAX_S)
        self.released()

    def until_restarted(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            for _ in range(GATE_LOOKS):
                self.advance(ra.GATE_POLL_S)
                if "restart_renderer" in self.ctl.calls:
                    return
        self.fail("no restart")

    def test_the_restart_gets_its_own_time_to_play(self):
        for fail in (None, OSError("speaker unreachable")):
            with self.subTest(fail=fail):
                self.setUp()
                self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                                     start_gate_seconds=12)
                self.ctl.renderer = "TRANSITIONING"
                self.ctl.restart_fail = fail  # a failed restart gets its time too
                self.held()
                self.until_restarted()
                self.assertEqual(self.clock.now, 1000 + 2 * ra.GATE_RETAKE_S)
                self.assertGreaterEqual(self.f._gate.deadline, self.clock.now + ra.GATE_RETAKE_S)
                self.gate_runs_out("WARNING")
                self.assertEqual(self.clock.now, 1000 + 3 * ra.GATE_RETAKE_S)
                self.released()

    def test_a_failing_retake_still_gets_its_time_and_keeps_the_deadlines_consistent(self):
        self.ctl.takeover_s = 3
        self.held()
        self.ctl.fail = OSError("speaker unreachable")
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            for _ in range(GATE_LOOKS):
                gate = self.f._gate
                self.assertLessEqual(self.f.deadline, gate.deadline)
                self.assertLessEqual(gate.deadline, gate.started + ra.START_GATE_MAX_S)
                self.advance(ra.GATE_POLL_S)
                if self.f._requests:
                    break
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), ra.GATE_MAX_RETAKES)
        self.assertTrue(any("speaker unreachable" in m for m in logs.output), logs.output)
        # Retakes at +8 s and +16 s fail after 3 s each; the last one still gets its time.
        self.assertEqual(self.clock.now, 1019 + ra.GATE_RETAKE_S)
        self.released()

    def test_gate_timeout_names_the_last_renderer_state_and_the_room_verdict(self):
        self.ctl.verdict = ra.STANDBY
        self.held()
        logs = self.gate_runs_out("WARNING")
        timeout = [m for m in logs if "anyway" in m]
        self.assertEqual(len(timeout), 1, logs)
        self.assertIn("renderer 'STOPPED'", timeout[0])
        self.assertIn("room 'standby'", timeout[0])
        self.assertTrue(timeout[0].startswith("WARNING:"), timeout)

    def test_gate_timeout_names_a_failing_renderer_check(self):
        self.held()
        self.ctl.plays_fail = OSError("connection refused")
        self.ctl.verdict = ra.NO_ZONE
        logs = self.gate_runs_out("WARNING")
        timeout = [m for m in logs if "anyway" in m][0]
        self.assertIn("connection refused", timeout)
        self.assertIn("room 'no_zone'", timeout)

    # -- after a gate timeout: a renderer that never started gets one more takeover

    def test_renderer_never_started_after_a_gate_timeout_gets_one_more_takeover(self):
        self.timed_out()
        self.ctl.verdict = ra.NOT_PLAYING
        self.poll(ra.STOPPED_GRACE_POLLS)
        self.assertEqual(self.requests(), [])
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.poll()
        self.assertEqual(self.ctl.calls[-1], "ensure_playing(retake)")
        self.assertTrue(any("never" in m for m in logs.output), logs.output)
        self.assertEqual(self.requests(), [])  # not taken away
        self.assertFalse(self.f.yielded)
        # Still stopped after that one: now the speaker counts as taken away.
        self.poll(ra.STOPPED_GRACE_POLLS)
        self.assertEqual(self.requests(), [])
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertEqual(self.ctl.calls.count("ensure_playing(retake)"), 1)

    def test_extra_takeover_finding_another_source_yields(self):
        self.timed_out()
        self.ctl.verdict = ra.NOT_PLAYING
        self.ctl.retake_decision = ra.OTHER_SOURCE  # someone switched it on just now
        self.poll(ra.STOPPED_GRACE_POLLS)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.ctl.calls[-1], "ensure_playing(retake)")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertTrue(self.f.yielded)
        self.assertIsNone(self.f.seconds_until_deadline())  # left alone from now on

    def test_renderer_that_ran_after_a_gate_timeout_and_then_stops_is_taken_away(self):
        for verdict in (ra.OURS, ra.TRANSITIONING):
            with self.subTest(verdict=verdict):
                self.setUp()
                self.timed_out()
                self.ctl.verdict = verdict
                self.poll()
                self.ctl.verdict = ra.NOT_PLAYING  # the user stopped it
                self.poll(ra.STOPPED_GRACE_POLLS)
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.poll()
                self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
                self.assertNotIn("ensure_playing(retake)", self.ctl.calls)

    def test_mpd_change_after_a_gate_timeout_drops_the_extra_takeover(self):
        self.timed_out()
        self.f.on_mpd_state("stop", None)
        self.f.on_mpd_state("play", "2")  # the phone skipped: restarts our stream, no wake
        self.assertEqual(self.ctl.calls, ["ensure_playing(no wake)"])
        self.ctl.verdict = ra.NOT_PLAYING
        self.poll(ra.STOPPED_GRACE_POLLS)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)

    def test_gate_ended_by_a_playing_renderer_gets_no_extra_takeover(self):
        self.held()
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released()
        self.ctl.verdict = ra.NOT_PLAYING
        self.poll(ra.STOPPED_GRACE_POLLS)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)

    def test_stop_from_the_phone_during_the_gate_is_not_overridden(self):
        self.held()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("stop", None)
        self.ctl.plays = True
        for _ in range(4):
            self.advance(5)
        self.assertEqual(self.requests(), [])  # never pause 0
        self.assertNotIn("renderer_state", self.ctl.calls)
        self.assertNotIn("ensure_playing(retake)", self.ctl.calls)  # no retake either

    # -- the receiver's pre-roll: the gate starts on silence, the song replaces it (MPD goes
    # play(silence) -> play(song), a new songid, no stop)

    def gate_fields(self):
        g = self.f._gate
        return (g.phase, g.started, g.deadline, g.taken, g.retakes, g.restarted)

    def test_song_arriving_during_the_gate_is_held_and_the_gate_carries_on(self):
        self.held("1")  # the pre-roll silence
        self.advance(ra.GATE_POLL_S)
        gate, fields = self.f._gate, self.gate_fields()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")  # the song replaces the silence
        self.assertIs(self.f._gate, gate)
        self.assertEqual(gate.song, "2")
        self.assertEqual(self.gate_fields(), fields)  # same timing and counters
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # held again, a moment in
        self.f.on_mpd_state("pause", "2")  # run() reports our pause 1
        self.assertEqual(self.requests(), [])
        # Not a takeover: no wake, no song-change takeover, the source before is kept.
        self.assertEqual(self.ctl.calls, ["ensure_playing", "renderer_state"])
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released("2")
        self.assertIsNone(self.f._gate)
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.ctl.calls, ["ensure_playing"] + ["renderer_state"] * 2)

    def test_song_arriving_while_mpd_is_paused_needs_no_new_pause(self):
        self.held("1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2")
        self.assertEqual(self.f._gate.song, "2")
        self.assertEqual(self.requests(), [])
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released("2")
        self.assertIsNone(self.f._gate)

    def test_song_arriving_before_our_pause_lands_is_held_by_it(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # sent ...
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")  # ... but the song's playid came after it
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # so pause again
        self.assertEqual(self.ctl.calls, [])
        self.f.on_mpd_state("pause", "2")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertEqual(self.f._gate.phase, ra._HOLDING)

    def test_song_arriving_while_our_pause_still_waits_to_go_out_is_not_paused_twice(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.f.requeue_mpd_requests(self.requests())  # lost connection before pause 1 went out
        self.f.on_mpd_connected()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])

    def test_reconnect_while_the_songs_pause_still_waits_to_go_out_keeps_the_gate(self):
        self.held("1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")  # the song arrives while MPD plays
        gate = self.f._gate
        self.f.requeue_mpd_requests(self.requests())  # lost connection before pause 1 went out
        self.f.on_mpd_connected()
        self.f.on_mpd_state("play", "2")  # the reconnect's first status: our pause is still due
        self.assertIs(self.f._gate, gate)
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.f.on_mpd_state("pause", "2")  # run() reports our pause 1
        self.assertIs(self.f._gate, gate)
        self.assertEqual(gate.phase, ra._HOLDING)
        self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)  # no new takeover

    # -- the receiver's held hand-over: when MPD is paused at the hand-over, the receiver runs
    # `playid <song>; pause 1; deleteid <silence>`, so MPD reports (pause, new songid) at 0:00

    def test_song_handed_over_held_during_the_takeover_is_the_held_song(self):
        self.ctl.takeover_s = 3  # the receiver hands over while the takeover blocks
        self.held("1")
        gate, fields = self.f._gate, self.gate_fields()
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("pause", "2", 0.0)  # read only after the takeover returned
        self.assertEqual(self.start_lines(logs.output),
                         ["start: song '2' at +3.0 s, speaker not playing yet: held"])
        self.assertIs(self.f._gate, gate)
        self.assertEqual(gate.song, "2")
        self.assertEqual(self.gate_fields(), fields)
        self.assertEqual(self.requests(), [])  # MPD already holds it: no extra pause
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released("2")
        self.assertIsNone(self.f._gate)
        self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)
        self.assertNotIn("ensure_playing(no wake)", self.ctl.calls)

    def resuming(self):
        """The gate on the silence ends: the speaker plays, our pause 0 goes out."""
        self.held("1")
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)

    def test_song_handed_over_held_while_our_resume_is_out_is_resumed_again(self):
        # Our pause 0 went out just before the receiver's list paused MPD at the song.
        self.resuming()
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])  # sent
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2", 0.0)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])  # pause 0 again
        self.assertEqual(self.f._gate.song, "2")
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "2")  # run() reports it
        self.assertIsNone(self.f._gate)
        self.assertTrue(any("MPD resumed" in m for m in logs.output), logs.output)
        self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)
        self.assertNotIn("ensure_playing(no wake)", self.ctl.calls)
        self.assertEqual(self.requests(), [])

    def test_song_handed_over_held_while_our_resume_still_waits_is_not_resumed_twice(self):
        self.resuming()
        self.f.requeue_mpd_requests(self.requests())  # lost connection before pause 0 went out
        self.f.on_mpd_connected()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2", 0.0)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])

    def resumed(self):
        """The gate on the silence ended: our pause 0 landed on the silence."""
        self.resuming()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.released("1")
        self.assertIsNone(self.f._gate)

    def test_song_handed_over_held_just_after_our_resume_landed_is_resumed(self):
        self.resumed()
        self.clock.now += 0.3
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("pause", "2", 0.0)
        self.assertTrue(any("song '2'" in m for m in self.start_lines(logs.output)), logs.output)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "2")  # run() reports our pause 0
        self.assertTrue(any("song '2' held" in m for m in logs.output), logs.output)
        self.assertIsNone(self.f._gate)
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)  # no new takeover
        self.assertNotIn("ensure_playing(no wake)", self.ctl.calls)
        self.assertEqual(self.f.pending, ra._POLL)  # watched as usual

    def test_a_pause_after_the_gate_that_is_no_held_hand_over_stays_paused(self):
        cases = {
            "too late": (ra.AFTER_GATE_HANDOVER_S + 0.1, "2", 0.0),
            # A literal time: the window stays short whatever the constant says.
            "6 s after our pause 0 landed": (6.0, "2", 0.0),
            "not at the start of the song": (0.3, "2", 5.0),
            "elapsed unknown": (0.3, "2", None),
            "same song (a pause from the phone)": (0.3, "1", 0.0),
        }
        for name, (after, song, elapsed) in cases.items():
            with self.subTest(name):
                self.setUp()
                self.resumed()
                self.clock.now += after
                self.f.on_mpd_state("pause", song, elapsed)
                self.assertEqual(self.requests(), [])
                self.assertIsNone(self.f._gate)

    def test_the_after_gate_window_is_a_few_seconds(self):
        self.assertLessEqual(ra.AFTER_GATE_HANDOVER_S, 5)

    def test_after_a_gate_whose_song_already_came_a_new_paused_song_is_left_paused(self):
        # A held hand-over only replaces the pre-roll silence; after the song came in the
        # gate, a paused new song is something else (e.g. the phone).
        self.held("1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2", 0.0)  # the song, handed over in the gate
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.released("2")
        self.assertIsNone(self.f._gate)
        self.clock.now += 0.3
        self.f.on_mpd_state("pause", "3", 0.0)
        self.assertEqual(self.requests(), [])
        self.assertIsNone(self.f._gate)

    def test_song_handed_over_with_our_pause_still_counts_as_the_songs_arrival(self):
        # The receiver's held hand-over and our pause 1 both landed before the status read:
        # MPD reports pause with the song's songid while the gate is still pausing.
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")  # the silence
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.clock.now += 0.4
        self.ctl.takeover_s = 2
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("pause", "2", 0.0)
        self.assertEqual(self.start_lines(logs.output)[0],
                         "start: song '2' at +0.4 s, speaker not playing yet: held")
        gate = self.f._gate
        self.assertEqual((gate.phase, gate.song, gate.song_at), (ra._HOLDING, "2", 1000.4))
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.released("2")
        self.assertTrue(any("song '2' held 2.2 s" in m for m in logs.output), logs.output)

    def test_the_preroll_song_after_a_gate_timeout_keeps_the_unstarted_takeover(self):
        for state, elapsed in (("play", None), ("pause", 0.0)):
            with self.subTest(state=state):
                self.setUp()
                self.timed_out()
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state(state, "2", elapsed)  # the pre-roll's song
                self.assertTrue(self.f._unstarted)
                if state == "pause":
                    self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
                    self.f.on_mpd_state("play", "2")
                    self.assertTrue(self.f._unstarted)

    def test_a_song_long_after_a_gate_timeout_drops_the_unstarted_takeover(self):
        self.timed_out()
        self.clock.now += ra.START_GATE_MAX_S + 1
        self.f.on_mpd_state("play", "2")
        self.assertFalse(self.f._unstarted)

    def test_song_arriving_keeps_the_retakes_and_the_unstarted_takeover(self):
        self.held("1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            while "ensure_playing(retake)" not in self.ctl.calls:
                self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.f._gate.retakes, 1)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")
        self.assertEqual(self.f._gate.retakes, 1)
        self.f.on_mpd_state("pause", "2")
        self.requests()
        self.gate_runs_out("WARNING")
        self.released("2")
        self.assertTrue(self.f._unstarted)

    def test_song_arriving_while_our_resume_is_on_its_way_ends_the_gate_and_plays(self):
        self.held("1")
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        resume = self.requests()
        self.assertEqual(resume, [ra.RESUME_PLAYER])
        for sent in (True, False):
            with self.subTest(resume_sent=sent):
                if not sent:
                    self.setUp()
                    self.held("1")
                    self.ctl.plays = True
                    with self.assertLogs("raumfeld_adapter", "INFO"):
                        self.advance(ra.GATE_POLL_S)
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state("play", "2")  # the speaker plays: the song may too
                self.assertIsNone(self.f._gate)
                self.assertEqual(self.requests(), [])  # no pause 1, and pause 0 is moot
                self.assertNotIn("ensure_playing(no wake)", self.ctl.calls)
                self.assertEqual(self.f.seconds_until_deadline(), 5)  # watched as usual

    # -- what a cast start took, phase by phase (INFO)

    def start_lines(self, output):
        return [m.split(":", 2)[2] for m in output if ":start: " in m]

    def test_a_cast_start_is_logged_phase_by_phase(self):
        self.ctl.takeover_s = 2.5
        self.ctl.last_takeover = ra.TakeoverStats(wake_s=2.1, lookups=0)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.held("1")  # the pre-roll silence
            self.clock.now += 0.5
            self.f.on_mpd_state("play", "2")  # the song, 3.0 s after MPD started
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.f.on_mpd_state("pause", "2")
            self.ctl.plays = True
            self.advance(1.0)
            self.released("2")
        self.assertEqual(self.start_lines(logs.output), [
            "start: takeover returned at +2.5 s (wake wait 2.1 s, cached addresses, no discovery)",
            "start: song '2' at +3.0 s, speaker not playing yet: held",
            "start: speaker plays at +4.0 s; resuming MPD",
            "start: MPD resumed at +4.0 s (song '2' held 1.0 s)",
        ])

    def test_a_takeover_that_had_to_discover_says_so(self):
        self.ctl.last_takeover = ra.TakeoverStats(wake_s=0.0, lookups=2)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.held()
        self.assertEqual(self.start_lines(logs.output), [
            "start: takeover returned at +0.0 s (wake wait 0.0 s, 2 discoveries)"])

    def test_one_discovery_is_singular(self):
        self.ctl.last_takeover = ra.TakeoverStats(wake_s=0.0, lookups=1)
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.held()
        self.assertEqual(self.start_lines(logs.output), [
            "start: takeover returned at +0.0 s (wake wait 0.0 s, 1 discovery)"])

    def test_a_song_after_the_gate_is_logged_with_the_speaker_already_playing(self):
        self.held("1")
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released("1")
        self.clock.now = 1006.0
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "2")
        self.assertEqual(self.start_lines(logs.output), [
            "start: song '2' at +6.0 s, speaker already playing: no hold"])
        self.assertEqual(self.requests(), [])
        with no_logs(self, "raumfeld_adapter", logging.INFO):
            self.f.on_mpd_state("play", "3")  # a later song is no cast start

    def test_a_song_long_after_the_gate_is_no_cast_start(self):
        self.held("1")
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.released("1")
        self.clock.now += ra.START_GATE_MAX_S + 1  # no pre-roll silence lasts that long
        with no_logs(self, "raumfeld_adapter", logging.INFO):
            self.f.on_mpd_state("play", "2")

    def test_user_skipping_during_the_gate_is_held_too(self):
        # Holding at the start of the new song never undoes what the user did.
        self.held("1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.f.on_mpd_state("pause", "2")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "3")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        self.assertEqual(self.f._gate.song, "3")
        self.assertEqual(self.ctl.calls.count("ensure_playing"), 1)

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
                # Another song is the pre-roll's song arriving: logged, held all the same.
                logs = (no_logs(self, "raumfeld_adapter", logging.INFO) if song == "1" else
                        self.assertLogs("raumfeld_adapter", "INFO"))
                with logs:
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
        self.ctl.pause_fail = ra.RaumfeldError("HTTP 500")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")  # the user pauses, the speaker plays on ...
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


class PauseFollowTest(unittest.TestCase):
    """The speaker buffers seconds of the stream: a user pause in MPD is followed to the zone
    renderer (AVTransport Pause) at once, and the resume plays it again behind the start
    gate, so its reconnect to the stream loses no audio."""

    GATE_S = 15

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.clock = self.clock
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock,
                             start_gate_seconds=self.GATE_S)

    def advance(self, seconds):
        self.clock.now += seconds
        self.f.tick()

    def requests(self):
        return self.f.take_mpd_requests()

    def owned(self, song="1"):
        """A cast that plays: gated, the renderer plays at the first look."""
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", song)
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.f.on_mpd_state("pause", song)
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.f.on_mpd_state("play", song)
        self.assertTrue(self.f.owned)
        self.ctl.calls.clear()

    def paused(self, song="1"):
        """The user pauses on the phone: the speaker is paused too."""
        self.owned(song)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", song)
        self.ctl.calls.clear()

    # -- pause

    def test_user_pause_pauses_the_speaker_at_once_and_says_so(self):
        self.owned()
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "pause_renderer"])
        self.assertTrue(any("INFO" in m and "Pause" in m for m in logs.output), logs.output)
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # watched as usual

    def test_the_gates_own_pause_is_not_followed_to_the_speaker(self):
        self.ctl.plays = False
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.f.on_mpd_state("pause", "1")  # our pause 1
        self.f.on_mpd_state("pause", "1")  # its late idle event
        self.assertNotIn("pause_renderer", self.ctl.calls)

    def test_the_receivers_held_handover_after_the_gate_is_not_followed(self):
        self.owned()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2", 0.0)  # playid; pause 1 at 0:00 of the song
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.assertEqual(self.ctl.calls, [])

    def test_a_pause_that_comes_with_another_song_is_not_followed(self):
        self.owned()
        self.clock.now += ra.AFTER_GATE_HANDOVER_S + 1
        self.f.on_mpd_state("pause", "2", 0.0)
        self.assertEqual(self.ctl.calls, [])

    def test_our_own_pause_after_a_yield_is_not_followed(self):
        self.owned()
        self.ctl.verdict = ra.STANDBY
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["check"])

    def test_a_speaker_not_playing_our_stream_is_not_paused(self):
        self.owned()
        self.ctl.plays = False
        for seen in ("PLAYING" + ra.ANOTHER_SOURCE, "STOPPED", "TRANSITIONING"):
            with self.subTest(seen=seen):
                self.ctl.renderer = seen
                self.ctl.calls.clear()
                self.f.on_mpd_state("pause", "1")
                self.assertEqual(self.ctl.calls, ["renderer_state"])
                self.f.on_mpd_state("play", "1")

    def test_a_failed_pause_is_logged_and_the_loop_goes_on(self):
        self.owned()
        self.ctl.pause_fail = ra.RaumfeldError("HTTP 500")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.assertEqual(self.requests(), [])
        # The speaker still plays: a resume is today's resume.
        self.ctl.calls.clear()
        self.f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertEqual(self.requests(), [])

    def test_a_failed_look_before_the_pause_is_logged_and_the_loop_goes_on(self):
        self.owned()
        self.ctl.plays_fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state"])
        self.assertEqual(self.f.seconds_until_deadline(), 5)

    # -- paused

    def test_our_paused_speaker_is_left_alone_while_mpd_stays_paused(self):
        self.paused()
        self.ctl.verdict = ra.NOT_PLAYING  # PAUSED_PLAYBACK on our stream
        for _ in range(ra.STOPPED_GRACE_POLLS + 5):
            self.advance(5)
        self.assertEqual(self.ctl.calls, ["check"] * (ra.STOPPED_GRACE_POLLS + 5))
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.f.yielded)
        self.assertTrue(self.f.owned)

    # -- resume

    def test_user_resume_plays_the_paused_speaker_behind_the_gate(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state"])
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # held at the resume point
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")  # our pause 1
        # Looked at again right before Play: still paused on our stream.
        self.assertEqual(self.ctl.calls, ["renderer_state", "renderer_state", "play_renderer"])
        # MPD is set back by what the speaker had buffered (ResumeCompensationTest).
        self.assertEqual(self.requests(), [ra.SEEK_BACK])
        self.ctl.renderer = "TRANSITIONING"  # reconnecting to the stream
        self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [])
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.f.on_mpd_state("play", "1")  # our pause 0
        self.f.on_mpd_state("play", "1")  # its idle event
        self.assertEqual(self.requests(), [])
        self.assertNotIn("ensure_playing", self.ctl.calls)  # no takeover, no wake
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.assertTrue(self.f.owned)

    def test_a_resume_gate_is_bounded_by_the_configured_gate(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.requests(), [ra.SEEK_BACK])
        self.ctl.renderer = "STOPPED"  # Play dropped
        with self.assertLogs("raumfeld_adapter", "INFO"):
            for _ in range(GATE_LOOKS):
                self.advance(ra.GATE_POLL_S)
                if self.f._requests:
                    break
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.assertIn("ensure_playing(retake)", self.ctl.calls)  # the gate's re-takeover

    def test_a_pause_from_the_phone_during_the_resume_gate_is_undone_like_at_a_start(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.f.on_mpd_state("pause", "1")
        self.assertNotIn("pause_renderer", self.ctl.calls)

    def test_resume_after_the_speaker_left_the_pause_takes_over_as_before(self):
        for seen in ("STOPPED", "NO_MEDIA_PRESENT", "PAUSED_PLAYBACK" + ra.NOTHING_LOADED):
            with self.subTest(seen=seen):
                self.setUp()
                self.paused()
                self.ctl.renderer = seen
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state("play", "1")
                self.assertEqual(self.ctl.calls, ["renderer_state", "ensure_playing"])
                self.assertEqual(self.requests(), [])  # an owned room: not gated, as before

    def test_resume_with_a_failing_look_takes_over_as_before(self):
        self.paused()
        self.ctl.plays_fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "ensure_playing"])

    def test_resume_after_the_user_chose_another_source_does_not_take_it_back(self):
        for seen in ra._OTHER_SOURCE_PLAYING + tuple(
                s + ra.PREVIOUS_SOURCE for s in ra._PLAYING_STATES):
            with self.subTest(seen=seen):
                self.setUp()
                self.paused()
                self.ctl.renderer = seen
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state("play", "1")
                self.assertEqual(self.ctl.calls, ["renderer_state"])
                self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
                self.assertTrue(self.f.yielded)

    def test_a_failed_play_on_resume_takes_over_within_the_gate(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.requests()
        self.ctl.play_fail = ra.RaumfeldError("HTTP 500")
        self.ctl.verdict = ra.NOT_PLAYING  # still ours, paused
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "renderer_state", "play_renderer",
                                          "check", "ensure_playing"])
        self.assertEqual(self.requests(), [])  # MPD stays held
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])

    def test_a_failed_play_and_a_failed_look_release_mpd(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.requests()
        self.ctl.play_fail = ra.RaumfeldError("HTTP 500")
        self.ctl.fail = OSError("unreachable")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])  # never keep the phone waiting
        self.assertNotIn("ensure_playing", self.ctl.calls)  # nothing taken over unseen
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # the polls decide

    def test_a_failed_play_on_a_room_taken_away_yields(self):
        for verdict in ra.TAKEN_AWAY:
            with self.subTest(verdict=verdict):
                self.setUp()
                self.paused()
                with self.assertLogs("raumfeld_adapter", "INFO"):
                    self.f.on_mpd_state("play", "1")
                self.requests()
                self.ctl.play_fail = ra.RaumfeldError("HTTP 500")
                self.ctl.verdict = verdict
                with self.assertLogs("raumfeld_adapter", "WARNING"):
                    self.f.on_mpd_state("pause", "1")
                self.assertEqual(self.requests(), [ra.RELEASE_CAST])  # MPD stays paused
                self.assertNotIn("ensure_playing", self.ctl.calls)
                self.assertTrue(self.f.yielded)

    def test_the_renderer_is_looked_at_again_right_before_play(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "renderer_state", "play_renderer"])

    def test_another_source_chosen_during_the_hold_is_not_taken_back(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.requests()
        self.ctl.renderer = "PLAYING" + ra.ANOTHER_SOURCE
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.requests(), [ra.RELEASE_CAST])
        self.assertNotIn("play_renderer", self.ctl.calls)
        self.assertNotIn("ensure_playing", self.ctl.calls)

    def test_a_renderer_no_longer_paused_at_play_is_looked_at_and_taken_over(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
        self.requests()
        self.ctl.renderer = "STOPPED"
        self.ctl.verdict = ra.NOT_PLAYING
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "renderer_state", "check",
                                          "ensure_playing"])

    # -- stop

    def test_stop_while_the_speaker_is_paused_sends_no_play_and_lets_go(self):
        self.paused()
        self.f.on_mpd_state("stop", None)
        self.assertFalse(self.f.owned)  # the renderer is paused on our stream, not playing
        self.assertIsNone(self.f.seconds_until_deadline())
        self.advance(5)
        self.assertEqual(self.ctl.calls, [])
        self.assertEqual(self.requests(), [])
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")  # another song (play_song: clear, add, play)
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # a start: through the gate
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])

    def test_another_song_while_the_speaker_is_paused_is_a_start_not_a_resume(self):
        self.paused()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")  # no stop seen in between
        self.assertEqual(self.ctl.calls, [])  # no resume look
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2")
        self.assertEqual(self.ctl.calls, ["ensure_playing"])
        self.assertNotIn("play_renderer", self.ctl.calls)

    # -- the speaker's own pause

    def test_a_pause_on_the_speaker_itself_is_not_taken_for_ours(self):
        self.owned()
        self.ctl.verdict = ra.NOT_PLAYING  # paused with its buttons; MPD still plays
        with self.assertLogs("raumfeld_adapter", "INFO"):
            for _ in range(ra.STOPPED_GRACE_POLLS + 1):
                self.advance(5)
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])  # as before
        self.f.on_mpd_state("pause", "1")  # our pause 1
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")  # the user resumes from the phone
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # a gated takeover, as before
        self.assertNotIn("pause_renderer", self.ctl.calls)
        self.assertNotIn("play_renderer", self.ctl.calls)

    # -- without the gate

    def test_without_the_gate_a_resume_plays_the_speaker_at_once(self):
        f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=0)
        f.on_mpd_state("play", "1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            f.on_mpd_state("pause", "1")
        self.ctl.calls.clear()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "play_renderer"])
        self.assertEqual(f.take_mpd_requests(), [])
        self.assertEqual(f.seconds_until_deadline(), 5)

    def test_without_the_gate_a_failed_play_takes_over_as_before(self):
        f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=0)
        f.on_mpd_state("play", "1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            f.on_mpd_state("pause", "1")
        self.ctl.calls.clear()
        self.ctl.play_fail = ra.RaumfeldError("HTTP 500")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            f.on_mpd_state("play", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "play_renderer", "ensure_playing"])


class VolumeFollowTest(unittest.TestCase):
    """The phone's volume (MPD's, a null mixer) is the speaker's own, scaled to 0..max_volume:
    a change is sent as the room's own volume (SetRoomVolume) at once; at a takeover
    and on the speaker's own changes the speaker's volume is given to MPD instead, so the
    phone shows it and the loudness does not jump."""

    MAX = 50

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.clock = self.clock
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=15,
                             max_volume=self.MAX)

    def advance(self, seconds):
        self.clock.now += seconds
        self.f.tick()

    def requests(self):
        return self.f.take_mpd_requests()

    def owned(self, song="1"):
        """A gated cast start; the speaker's volume goes to MPD (served like run() does)."""
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", song)
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.f.on_mpd_state("pause", song)
            self.mpd_volume_set()
            self.advance(ra.GATE_POLL_S)
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.f.on_mpd_state("play", song)
        self.ctl.volume_calls.clear()

    def mpd_volume_set(self):
        """run() gives MPD the volume (setvol); MPD reports the change back (idle mixer)."""
        volume = self.f.take_mpd_volume()
        self.f.on_mpd_volume(volume)
        return volume

    # -- cast start

    def test_takeover_gives_mpd_the_speakers_volume_and_leaves_the_speaker_alone(self):
        self.ctl.speaker_volume = 20
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])
        self.assertEqual(self.requests(), [])  # MPD's volume has its own channel
        self.assertEqual(self.mpd_volume_set(), 40)  # 20 of 50: the phone shows 40 %
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])  # no echo back
        self.assertTrue(any("volume" in m for m in logs.output), logs.output)

    def test_a_speaker_louder_than_max_volume_shows_100_and_is_left_alone(self):
        self.ctl.speaker_volume = 80
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.mpd_volume_set(), 100)
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])
        self.assertEqual(self.ctl.speaker_volume, 80)
        # Until the user moves the slider: then it is scaled, never above max_volume.
        self.f.on_mpd_volume(99)
        self.assertEqual(self.ctl.volume_calls, ["get_volume", "set_volume 50"])

    def test_the_phones_volume_is_not_pushed_at_a_takeover(self):
        # The phone's setVolume before the speaker is ours: nothing is sent to the speaker.
        self.f.on_mpd_volume(90)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.f.on_mpd_volume(80)  # during the gate, before the takeover
        self.assertEqual(self.ctl.volume_calls, [])

    def test_a_failed_volume_read_at_takeover_is_logged_and_the_cast_goes_on(self):
        self.ctl.volume_fail = ra.RaumfeldError("HTTP 500")
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertTrue(any("WARNING" in m for m in logs.output), logs.output)
        self.assertIsNone(self.f.take_mpd_volume())  # MPD's volume left as it is
        self.assertTrue(self.f.owned)
        self.assertEqual(self.f.seconds_until_deadline(), ra.GATE_POLL_S)  # the gate goes on

    # -- phone changes

    def test_a_phone_volume_change_is_sent_to_the_speaker_scaled(self):
        self.owned()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_volume(70)
        self.assertEqual(self.ctl.volume_calls, ["set_volume 35"])
        self.assertEqual(self.requests(), [])
        self.f.on_mpd_volume(100)
        self.f.on_mpd_volume(0)
        self.assertEqual(self.ctl.volume_calls, ["set_volume 35", "set_volume 50", "set_volume 0"])

    def test_phone_changes_are_sent_while_paused_too(self):
        self.owned()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
        self.f.on_mpd_volume(20)
        self.assertEqual(self.ctl.volume_calls, ["set_volume 10"])

    def test_no_volume_is_sent_without_a_volume_from_mpd(self):
        self.owned()
        self.f.on_mpd_volume(None)  # MPD without a mixer reports none
        self.assertEqual(self.ctl.volume_calls, [])

    def test_a_failed_set_volume_is_logged_and_the_loop_goes_on(self):
        self.owned()
        self.ctl.volume_fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.f.on_mpd_volume(70)
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.ctl.volume_fail = None
        self.f.on_mpd_volume(60)
        self.assertEqual(self.ctl.volume_calls, ["set_volume 35", "set_volume 30"])

    # -- speaker changes

    def test_a_volume_changed_on_the_speaker_is_given_to_mpd_without_an_echo(self):
        self.owned()
        self.ctl.speaker_volume = 45  # its buttons, or the Raumfeld app
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])
        self.assertEqual(self.mpd_volume_set(), 90)
        self.advance(5)  # unchanged: nothing more for MPD
        self.assertIsNone(self.f.take_mpd_volume())
        self.assertEqual(self.ctl.volume_calls, ["get_volume", "get_volume"])  # no SetVolume

    def test_a_failed_volume_poll_is_logged_and_polling_goes_on(self):
        self.owned()
        self.ctl.volume_fail = OSError("timeout")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.advance(5)
        self.assertEqual(self.f.seconds_until_deadline(), 5)
        self.assertIsNone(self.f.take_mpd_volume())

    def test_a_volume_not_given_to_mpd_is_given_after_a_reconnect(self):
        self.owned()
        self.ctl.speaker_volume = 45
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.f.requeue_mpd_volume(self.f.take_mpd_volume())  # the connection dropped
        self.assertEqual(self.mpd_volume_set(), 90)
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])

    def test_only_our_own_setvol_is_not_echoed(self):
        self.owned()
        self.ctl.speaker_volume = 45
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)
        self.f.take_mpd_volume()  # setvol 90 sent; the phone moves before its event is read
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_volume(60)
        self.f.on_mpd_volume(60)
        self.assertEqual(self.ctl.volume_calls, ["get_volume", "set_volume 30", "set_volume 30"])

    # -- not ours

    def test_no_volume_calls_when_the_speaker_is_not_ours(self):
        self.owned()
        self.ctl.verdict = ra.OTHER_SOURCE
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.advance(5)  # yields
        self.f.on_mpd_volume(70)
        self.f.on_mpd_state("pause", "1")
        self.f.on_mpd_volume(60)
        self.assertEqual(self.ctl.volume_calls, [])

    def test_no_volume_calls_when_mpd_is_stopped(self):
        self.owned()
        self.f.on_mpd_state("stop", None)
        self.ctl.speaker_volume = 10
        self.advance(5)
        self.f.on_mpd_volume(70)
        self.assertEqual(self.ctl.volume_calls, [])

    def test_no_volume_calls_without_a_cast(self):
        self.f.on_mpd_volume(70)
        self.advance(5)
        self.assertEqual(self.ctl.volume_calls, [])

    def test_the_gate_and_the_hand_over_make_no_volume_calls(self):
        self.ctl.plays = False
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
            self.mpd_volume_set()
            self.f.on_mpd_state("pause", "2")  # the receiver's held hand-over
            for _ in range(GATE_LOOKS):  # gate checks, re-takeovers, timeout
                self.advance(ra.GATE_POLL_S)
                if ra.RESUME_PLAYER in self.f._requests:
                    break
        self.assertEqual(self.ctl.volume_calls, ["get_volume"])  # the takeover's read only
        self.assertIn("ensure_playing(retake)", self.ctl.calls)

    def test_a_resume_of_our_paused_speaker_reads_no_volume(self):
        self.owned()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.volume_calls, [])

    def test_max_volume_defaults_like_the_config(self):
        self.assertEqual(ra.Follower(self.ctl).max_volume, ra.DEFAULT_MAX_VOLUME)
        self.assertEqual(ra.DEFAULT_MAX_VOLUME, 50)



class FlacReleaseTest(unittest.TestCase):
    """A FLAC stream carries no data while MPD is paused, so a speaker waiting for it would
    only start when the gate gives up. With FLAC MPD is held only while the takeover or the
    resume Play runs, and released right after it returned; a renderer that then never
    plays gets one more takeover, a stuck one the usual restarts."""

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.clock = self.clock
        self.ctl.plays = False  # connecting
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=15,
                             hold_until_playing=False)

    def advance(self, seconds):
        self.clock.now += seconds
        self.f.tick()

    def requests(self):
        return self.f.take_mpd_requests()

    def started(self, song="1"):
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", song)
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.assertEqual(self.ctl.calls, [])
            self.f.on_mpd_state("pause", song)
        return logs.output

    def test_a_start_releases_mpd_right_after_the_takeover(self):
        logs = self.started()
        self.assertEqual(self.ctl.calls, ["ensure_playing"])  # no wait for PLAYING
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.assertTrue(any("released right after" in m for m in logs), logs)
        self.f.on_mpd_state("play", "1")  # our pause 0
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.f.seconds_until_deadline(), 5)  # watched as usual
        self.assertTrue(self.f.owned)

    def test_a_renderer_that_never_plays_gets_one_more_takeover(self):
        self.started()
        self.requests()
        self.f.on_mpd_state("play", "1")
        self.ctl.calls.clear()
        self.ctl.verdict = ra.NOT_PLAYING
        with self.assertLogs("raumfeld_adapter", "INFO"):
            for _ in range(ra.STOPPED_GRACE_POLLS + 1):
                self.advance(5)
        self.assertIn("ensure_playing(retake)", self.ctl.calls)
        self.assertEqual(self.requests(), [])  # not taken away yet

    def test_a_renderer_stuck_after_the_release_is_restarted(self):
        self.started()
        self.requests()
        self.f.on_mpd_state("play", "1")
        self.ctl.verdict = ra.TRANSITIONING
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            for _ in range(int(ra.STUCK_S / 5) + 2):
                self.advance(5)
        self.assertIn("restart_renderer", self.ctl.calls)

    def test_a_held_hand_over_after_the_release_is_resumed(self):
        self.started()
        self.requests()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "2", 0.0)  # the receiver hands its song over held
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])

    def test_a_failed_takeover_releases_mpd_and_is_retried(self):
        self.ctl.fail = OSError("unreachable")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.assertEqual(self.f.seconds_until_deadline(), ra.PLAY_RETRY_S)

    def test_a_resume_releases_mpd_right_after_play(self):
        self.ctl.plays = True
        self.started()
        self.requests()
        self.f.on_mpd_state("play", "1")
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")  # the user: the speaker is paused too
        self.ctl.calls.clear()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.ctl.calls, ["renderer_state", "renderer_state", "play_renderer"])
        self.assertEqual(self.requests(), [ra.SEEK_BACK, ra.RESUME_PLAYER])



class NoRenderingControlTest(unittest.TestCase):
    def test_a_renderer_without_rendering_control_gets_no_volume_calls_or_warnings(self):
        clock = FakeClock()
        ctl = RecordingController()
        ctl.rendering = False
        f = ra.Follower(ctl, poll_seconds=5, clock=clock, start_gate_seconds=0)
        with no_logs(self, "raumfeld_adapter", logging.WARNING):
            f.on_mpd_state("play", "1")
            for _ in range(3):
                clock.now += 5
                f.tick()
            f.on_mpd_volume(70)
        self.assertEqual(ctl.volume_calls, [])



class RelTimeTest(unittest.TestCase):
    def test_reltime_is_parsed_to_seconds(self):
        self.assertEqual(ra.reltime_seconds("0:01:05"), 65)
        self.assertEqual(ra.reltime_seconds("1:02:03"), 3723)
        self.assertEqual(ra.reltime_seconds("00:00:07.500"), 7.5)
        for bad in ("", "NOT_IMPLEMENTED", "1:60:00", "-0:00:01", "0:1:2", "x" * 70):
            self.assertIsNone(ra.reltime_seconds(bad), bad)


class ResumeCompensationTest(unittest.TestCase):
    """The speaker has some of the stream buffered when it is paused, and drops it when Play
    reconnects it: MPD is that much ahead of what was heard. At the pause the lead is
    measured (time since our Play connected it, less RelTime); at the resume of the same song
    MPD is set back by it (seekcur, while still held) before it is released."""

    def setUp(self):
        self.clock = FakeClock()
        self.ctl = RecordingController()
        self.ctl.clock = self.clock
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=15,
                             hold_until_playing=False)

    def requests(self):
        return self.f.take_mpd_requests()

    def playing(self):
        """A FLAC cast start: the speaker is connected at the takeover's Play."""
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertEqual(self.requests(), [ra.RESUME_PLAYER])
        self.f.on_mpd_state("play", "1")
        self.ctl.calls.clear()

    def paused_after(self, seconds, heard):
        self.playing()
        self.clock.now += seconds
        self.ctl.heard = heard
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("pause", "1")
        return logs.output

    def resumed(self):
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.f.on_mpd_state("play", "1")
            self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
            self.f.on_mpd_state("pause", "1")
        return logs.output

    def test_the_measured_lead_is_taken_back_at_the_resume(self):
        logs = self.paused_after(100, 98)  # 2 s not heard, less half RelTime's resolution
        self.assertTrue(any("1.5 s" in m for m in logs), logs)
        self.resumed()
        self.assertEqual(self.requests(), [ra.SEEK_BACK, ra.RESUME_PLAYER])  # seek first
        self.assertEqual(self.f.take_seek_back(), 1.5)
        self.assertIsNone(self.f.take_seek_back())  # handed out once

    def test_the_lead_is_measured_before_the_pause(self):
        self.paused_after(100, 98)
        self.assertEqual(self.ctl.position_calls, [self.clock.now])
        self.assertEqual(self.ctl.calls, ["renderer_state", "pause_renderer"])

    def test_an_unknown_or_odd_lead_is_one_second(self):
        for heard, fail in ((None, None), (98, OSError("timeout")), (100, None), (90, None),
                            (99.5, None)):
            with self.subTest(heard=heard, fail=fail):
                self.setUp()
                self.ctl.position_fail = fail
                self.paused_after(100, heard)
                self.resumed()
                self.assertEqual(self.requests(), [ra.SEEK_BACK, ra.RESUME_PLAYER])
                self.assertEqual(self.f.take_seek_back(), ra.DEFAULT_LEAD_S)

    def test_a_resume_after_a_retake_measures_from_the_new_connection(self):
        self.paused_after(100, 98)
        self.resumed()
        self.requests()
        self.f.take_seek_back()
        self.f.on_mpd_state("play", "1")  # our pause 0: the speaker reconnected at Play
        self.ctl.calls.clear()
        self.ctl.plays = True
        self.clock.now += 30
        self.ctl.heard = 28  # 30 s since the resume Play, 28 heard
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
        self.resumed()
        self.requests()
        self.assertEqual(self.f.take_seek_back(), 1.5)

    def test_no_compensation_for_another_song(self):
        self.paused_after(100, 98)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "2")
            self.requests()
            self.f.on_mpd_state("pause", "2")
        self.assertNotIn(ra.SEEK_BACK, self.requests())
        self.assertIsNone(self.f.take_seek_back())

    def test_no_compensation_when_the_speaker_left_the_pause(self):
        self.paused_after(100, 98)
        self.ctl.renderer = "STOPPED"
        self.ctl.verdict = ra.NOT_PLAYING
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
        self.assertNotIn(ra.SEEK_BACK, self.requests())
        self.assertIsNone(self.f.take_seek_back())

    def test_no_compensation_after_a_pause_that_was_not_followed(self):
        self.playing()
        self.ctl.plays, self.ctl.renderer = False, "STOPPED"
        self.f.on_mpd_state("pause", "1")
        self.f.on_mpd_state("play", "1")
        self.assertNotIn(ra.SEEK_BACK, self.requests())

    def test_no_compensation_without_the_gate(self):
        f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=0)
        f.on_mpd_state("play", "1")
        self.clock.now += 100
        self.ctl.heard = 98
        with self.assertLogs("raumfeld_adapter", "INFO"):
            f.on_mpd_state("pause", "1")
            f.on_mpd_state("play", "1")
        self.assertEqual(f.take_mpd_requests(), [])
        self.assertIsNone(f.take_seek_back())

    def test_the_lead_is_taken_back_with_the_hold_too(self):
        self.f = ra.Follower(self.ctl, poll_seconds=5, clock=self.clock, start_gate_seconds=15)
        self.ctl.plays = True
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("play", "1")
            self.requests()
            self.f.on_mpd_state("pause", "1")
            self.f.tick()
        self.requests()
        self.f.on_mpd_state("play", "1")
        self.clock.now += 100
        self.ctl.heard = 98
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.f.on_mpd_state("pause", "1")
        self.resumed()
        self.assertEqual(self.requests(), [ra.SEEK_BACK])  # pause 0 once the speaker plays
        self.assertEqual(self.f.take_seek_back(), 1.5)


if __name__ == "__main__":
    unittest.main()
