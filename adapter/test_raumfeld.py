"""Feature: Raumfeld follows the cast -- Controller against a fake Raumfeld host."""

import unittest
import xml.etree.ElementTree as ET

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, NEW_ZONE_UDN, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN,
                   FakeDiscovery, FakeRaumfeld)


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
        # Scenario: never volume -- in every scenario.
        for method, path, action, body in self.rf.requests:
            self.assertNotIn("RenderingControl", path)
            self.assertNotIn("Volume", path + (action or "") + body)

    def test_cast_starts_speaker_in_standby_reuses_existing_zone(self):
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.api_calls(), ["getZones", "leaveStandby"])
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
        self.assertEqual(calls[:3], ["getZones", "leaveStandby", "connectRoomToZone"])
        self.assertEqual(calls[3:], ["getZones"] * 4)
        self.assertEqual(self.sleeps, [0.5] * 3)
        self.assertEqual(self.discover.searches[-1], NEW_ZONE_UDN)
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

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

    def test_line_in_active_is_not_taken_over(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.ctl.ensure_playing()
        self.assertTrue(any("line-in" in m.lower() for m in logs.output), logs.output)
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
        self.assertEqual(self.rf.api_calls(), ["getZones"])
        self.assertEqual(self.rf.current_uri, LINE_IN_URI)

    def test_release_stops_and_enters_standby_when_playing_our_stream(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = STREAM_URL
        self.ctl.release()
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo", "Stop"])
        self.assertEqual(self.rf.api_calls(), ["getZones", "enterManualStandby"])
        self.assertEqual(self.rf.power, "MANUAL_STANDBY")

    def test_release_leaves_other_playback_alone(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        self.ctl.release()
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
        self.assertEqual(self.rf.api_calls(), ["getZones"])
        self.assertEqual(self.rf.power, "ACTIVE")

    def test_release_without_zone_or_in_standby_does_nothing(self):
        self.ctl.release()  # MANUAL_STANDBY
        self.rf.zone_udn = None
        self.rf.power = "ACTIVE"
        self.ctl.release()
        self.assertEqual(self.rf.soap_actions(), [])
        self.assertEqual(self.rf.api_calls(), ["getZones", "getZones"])

    def test_speaker_not_found_raises_device_not_found(self):
        self.discover.speaker_present = False
        with self.assertRaises(ra.DeviceNotFound):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.requests, [])

    def test_http_error_raises_os_error(self):
        self.rf.fail_paths.add("leaveStandby")
        with self.assertRaises(OSError):
            self.ctl.ensure_playing()
        self.rf.fail_paths.clear()
        self.ctl.ensure_playing()  # next attempt works
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_rediscovers_on_every_action(self):
        self.ctl.ensure_playing()
        self.ctl.release()
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


if __name__ == "__main__":
    unittest.main()
