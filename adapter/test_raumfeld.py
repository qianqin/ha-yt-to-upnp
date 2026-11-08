"""Feature: Raumfeld follows the cast -- Controller against a fake Raumfeld host."""

import os
import unittest
import xml.etree.ElementTree as ET

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, NEW_ZONE_UDN, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN,
                   FakeDiscovery, FakeRaumfeld)
from test_speaker_yield import assert_speaker_left_alone


class ControllerTest(unittest.TestCase):
    metadata = True

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.discover = FakeDiscovery(self.rf)
        self.sleeps = []
        config = ra.Config.from_env({
            "RAUMFELD_ROOM_UDN": ROOM_UDN,
            "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
            "STREAM_URL": STREAM_URL,
            "STREAM_MIME": "audio/mpeg",
            "STREAM_METADATA": "1" if self.metadata else "0",
        })
        self.ctl = ra.Controller(config, self.discover, api_port=self.rf.port,
                                 sleep=self.sleeps.append, zone_wait_s=10, zone_poll_s=0.5)

    def tearDown(self):
        # Scenario: never volume, never Stop or standby -- in every scenario.
        assert_speaker_left_alone(self, self.rf)

    def test_cast_starts_speaker_in_standby_reuses_existing_zone(self):
        self.ctl.ensure_playing()
        # The room is seen awake (ACTIVE, in its zone) before the renderer is set up.
        self.assertEqual(self.rf.api_calls(), ["getZones", "leaveStandby", "getZones"])
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])

    def test_active_idle_zone_is_queried_then_taken_over(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri = "http://other.invalid/old.mp3"
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(),
                         ["GetTransportInfo", "GetMediaInfo", "SetAVTransportURI", "Play"])
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())

    def test_no_zone_connects_once_and_polls_until_zone_appears(self):
        self.rf.zone_udn = None
        self.rf.polls_until_new_zone = 3
        self.ctl.ensure_playing()
        calls = self.rf.api_calls()
        self.assertEqual(calls.count("connectRoomToZone"), 1)
        # A room without a zone is not waited for to show up in one: it is connected.
        self.assertEqual(calls[:4], ["getZones", "leaveStandby", "getZones", "connectRoomToZone"])
        self.assertEqual(calls[4:], ["getZones"] * 4)
        self.assertEqual(self.sleeps, [0.5] * 3)
        self.assertEqual(self.discover.searches[-1], NEW_ZONE_UDN)
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

    def test_wake_waits_until_the_room_reports_active_in_its_zone(self):
        self.rf.wake_after_polls = 3
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.api_calls(), ["getZones", "leaveStandby"] + ["getZones"] * 4)
        self.assertEqual(self.sleeps, [ra.WAKE_POLL_S] * 3)
        last_look = max(i for i, r in enumerate(self.rf.requests) if r[1].endswith("getZones"))
        first_soap = min(i for i, r in enumerate(self.rf.requests) if r[2])
        self.assertLess(last_look, first_soap)  # nothing sent to a room still asleep
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

    def test_wake_resolves_zone_and_renderer_afresh(self):
        # Whatever was known before the wake may belong to a speaker still shutting down.
        self.rf.wake_after_polls = 1
        self.rf.zone_after_wake = NEW_ZONE_UDN
        self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, NEW_ZONE_UDN])
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_room_not_awake_in_time_is_taken_over_anyway_with_a_warning(self):
        self.rf.wake_after_polls = 1000
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            self.ctl.ensure_playing()
        self.assertTrue(any("anyway" in m for m in logs.output), logs.output)
        self.assertEqual(len(self.sleeps), int(ra.WAKE_WAIT_S / ra.WAKE_POLL_S))
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

    def test_active_room_is_not_waited_for(self):
        self.rf.power = "ACTIVE"
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.api_calls(), ["getZones", "leaveStandby"])
        self.assertEqual(self.sleeps, [])

    def test_zone_never_appears_raises_after_wait(self):
        self.rf.zone_udn = None
        self.rf.polls_until_new_zone = 1000
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.api_calls().count("connectRoomToZone"), 1)
        self.assertEqual(len(self.sleeps), 20)
        self.assertEqual(self.rf.soap_actions(), [])

    def test_already_playing_our_stream_only_queries(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = STREAM_URL
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
        self.assertEqual(self.rf.api_calls(), ["getZones"])

    def test_a_cast_takes_over_line_in_even_while_it_plays(self):
        # Home Assistant leaves the room on line-in, which reports PLAYING without audio.
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.assertEqual(self.ctl.ensure_playing(), ra.TAKE_OVER)
        self.assertTrue(any("line-in" in m.lower() for m in logs.output), logs.output)
        self.assertEqual(self.rf.soap_actions(),
                         ["GetTransportInfo", "GetMediaInfo", "SetAVTransportURI", "Play"])
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_a_cast_takes_over_another_playing_stream(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = "http://radio.invalid/live.mp3"
        self.assertEqual(self.ctl.ensure_playing(), ra.TAKE_OVER)
        self.assertEqual(self.rf.current_uri, STREAM_URL)

    def test_takeover_reports_its_decision(self):
        self.assertEqual(self.ctl.ensure_playing(), ra.TAKE_OVER)
        self.assertEqual(self.ctl.ensure_playing(), ra.ALREADY_PLAYING)
        self.rf.current_uri = LINE_IN_URI
        self.assertEqual(self.ctl.ensure_playing(), ra.TAKE_OVER)

    def test_without_wake_a_room_in_standby_is_left_off(self):
        self.assertEqual(self.ctl.ensure_playing(wake=False), ra.STANDBY)
        self.assertEqual(self.rf.api_calls(), ["getZones"])
        self.assertEqual(self.rf.soap_actions(), [])
        self.assertEqual(self.rf.power, "MANUAL_STANDBY")

    def test_without_wake_another_source_is_left_alone(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri = "http://other.invalid/old.mp3"  # even when stopped
        self.assertEqual(self.ctl.ensure_playing(wake=False), ra.OTHER_SOURCE)
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])

    def test_without_wake_our_stopped_stream_is_restarted(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri = STREAM_URL
        self.assertEqual(self.ctl.ensure_playing(wake=False), ra.TAKE_OVER)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_check_reports_the_speaker_verdict_and_never_acts(self):
        self.assertEqual(self.ctl.check(), ra.STANDBY)
        self.rf.power = "ACTIVE"
        self.rf.current_uri = STREAM_URL
        self.assertEqual(self.ctl.check(), ra.NOT_PLAYING)
        self.rf.transport_state = "PLAYING"
        self.assertEqual(self.ctl.check(), ra.OURS)
        self.rf.transport_state = "TRANSITIONING"
        self.assertEqual(self.ctl.check(), ra.TRANSITIONING)
        self.rf.current_uri = LINE_IN_URI
        self.assertEqual(self.ctl.check(), ra.OTHER_SOURCE)
        self.rf.zone_udn = None
        self.assertEqual(self.ctl.check(), ra.NO_ZONE)
        self.assertEqual(set(self.rf.api_calls()), {"getZones"})
        self.assertEqual(set(self.rf.soap_actions()), {"GetTransportInfo", "GetMediaInfo"})

    def test_check_reuses_discovery_until_the_zone_changes(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()
        self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.rf.zone_udn = NEW_ZONE_UDN
        self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN, NEW_ZONE_UDN])

    def test_check_rediscovers_after_an_error(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()
        self.rf.fail_paths.add("AVTransport")
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.check()
        self.rf.fail_paths.clear()
        self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN] * 2)

    def test_renderer_state_names_the_state_and_flags_another_source(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()  # the zone renderer is known from now on
        self.rf.requests.clear()
        for state, uri, seen in (("PLAYING", STREAM_URL, "PLAYING"),
                                 # line-in reports PLAYING
                                 ("PLAYING", LINE_IN_URI, "PLAYING (another source)"),
                                 ("TRANSITIONING", STREAM_URL, "TRANSITIONING"),
                                 ("STOPPED", STREAM_URL, "STOPPED"),
                                 ("STOPPED", "", "STOPPED (another source)")):
            with self.subTest(state=state, uri=uri):
                self.rf.transport_state, self.rf.current_uri = state, uri
                self.assertEqual(self.ctl.renderer_state(), seen)
        self.assertEqual(self.rf.api_calls(), [])  # the renderer only, no getZones
        self.assertEqual(set(self.rf.soap_actions()), {"GetTransportInfo", "GetMediaInfo"})

    def test_renderer_state_without_a_known_zone_raises(self):
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.renderer_state()

    def test_forced_takeover_sets_up_a_transitioning_renderer_afresh(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = STREAM_URL, "TRANSITIONING"
        self.assertEqual(self.ctl.ensure_playing(), ra.ALREADY_PLAYING)  # unforced: left alone
        self.assertNotIn("Play", self.rf.soap_actions())
        self.assertEqual(self.ctl.ensure_playing(force=True), ra.TAKE_OVER)
        self.assertEqual(self.rf.soap_actions()[-2:], ["SetAVTransportURI", "Play"])
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_forced_takeover_leaves_a_playing_renderer_alone(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = STREAM_URL, "PLAYING"
        self.assertEqual(self.ctl.ensure_playing(force=True), ra.ALREADY_PLAYING)
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
        self.assertEqual(self.rf.api_calls(), ["getZones"])  # no leaveStandby either

    def test_speaker_not_found_raises_device_not_found(self):
        self.discover.speaker_present = False
        with self.assertRaises(ra.DeviceNotFound):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.requests, [])

    def test_http_error_raises(self):
        self.rf.fail_paths.add("leaveStandby")
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.ensure_playing()
        self.rf.fail_paths.clear()
        self.ctl.ensure_playing()  # next attempt works
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_zone_location_must_be_http_on_the_ssdp_sender(self):
        self.rf.power = "ACTIVE"
        # A small local file and a refused port keep a regression from hanging the suite.
        for location in ("file://" + os.path.abspath(__file__), "http://[::1]:%d/zone.xml" % self.rf.port):
            self.discover.zone_location = location
            with self.subTest(location=location), self.assertRaises(ra.RaumfeldError):
                self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])

    def test_control_url_must_point_at_the_same_host(self):
        self.rf.power = "ACTIVE"
        self.rf.control_url = "http://[::1]:%d/AVTransport/ctrl" % self.rf.port
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])

    def test_api_redirect_to_another_host_is_refused(self):
        self.rf.redirect_base = "http://[::1]:%d%s" % (self.rf.port, "/x")
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.ensure_playing()
        self.assertEqual(len(self.rf.requests), 1)

    def test_takeover_rediscovers_every_time(self):
        self.ctl.ensure_playing()
        self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches.count(SPEAKER_UDN), 2)
        self.assertEqual(self.discover.searches.count(ZONE_UDN), 2)

    def test_metadata_is_escaped_didl_in_soap_body(self):
        self.ctl.ensure_playing()
        body = self.rf.soap_body("SetAVTransportURI")
        self.assertIn("&lt;DIDL-Lite", body)
        args = {el.tag: el.text or "" for el in ET.fromstring(body).iter()}
        self.assertEqual(args["CurrentURI"], STREAM_URL)
        self.assertEqual(args["CurrentURIMetaData"], ra.build_didl(STREAM_URL, "audio/mpeg"))
        self.assertIn('protocolInfo="http-get:*:audio/mpeg:*"', args["CurrentURIMetaData"])


class ControllerWithoutMetadataTest(ControllerTest):
    metadata = False

    def test_metadata_is_escaped_didl_in_soap_body(self):
        self.ctl.ensure_playing()
        args = {el.tag: el.text or "" for el in ET.fromstring(self.rf.soap_body("SetAVTransportURI")).iter()}
        self.assertEqual(args["CurrentURI"], STREAM_URL)
        self.assertEqual(args["CurrentURIMetaData"], "")


class RendererRestartTest(unittest.TestCase):
    """Scenario: a zone renderer stuck in TRANSITIONING (it dropped the stream) is brought
    back with Stop then Play on the zone it was last seen in; a plain Play does not help."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.discover = FakeDiscovery(self.rf)
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        self.ctl = ra.Controller(config, self.discover, api_port=self.rf.port,
                                 sleep=lambda s: None)

    def test_restart_sends_stop_then_play_to_the_checked_zone(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri = STREAM_URL
        self.rf.transport_state = "TRANSITIONING"
        self.rf.stuck = True
        self.assertEqual(self.ctl.check(), ra.TRANSITIONING)
        self.rf.requests.clear()
        self.ctl.restart_renderer()
        self.assertEqual(self.rf.soap_actions(), ["Stop", "Play"])
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.rf.current_uri, STREAM_URL)  # the URI is left as it is
        self.assertEqual(self.rf.api_calls(), [])  # no wake, no zone change, no volume

    def test_restart_without_a_checked_zone_raises(self):
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.restart_renderer()
        self.assertEqual(self.rf.requests, [])


if __name__ == "__main__":
    unittest.main()
