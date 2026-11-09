"""Feature: Raumfeld follows the cast -- Controller against a fake Raumfeld host."""

import os
import socket
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, NEW_ZONE_UDN, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN,
                   FakeDiscovery, FakeRaumfeld, FakeSsdpResponder)
from test_speaker_yield import assert_speaker_left_alone

OTHER_URI = "http://radio.invalid/live.mp3"


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
        self.now = 1000.0  # sleeping moves the controller's clock on
        self.ctl = ra.Controller(config, ra.SsdpLocator(self.discover, SPEAKER_UDN),
                                 api_port=self.rf.port,
                                 sleep=self.sleep, zone_wait_s=10, zone_poll_s=0.5,
                                 clock=lambda: self.now)

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds

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

    def test_a_room_waking_in_another_zone_uses_that_zones_renderer(self):
        # The zone seen before the wake may belong to a speaker still shutting down.
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

    def test_wake_wait_is_bounded_by_time_not_by_the_number_of_looks(self):
        self.rf.wake_after_polls = 1000
        start = self.now

        def slow_answer():  # each getZones takes 3 s
            self.now += 3
        self.rf.on_get_zones = slow_answer
        with mock.patch.object(ra, "http_get", wraps=ra.http_get) as get:
            with self.assertLogs("raumfeld_adapter", "WARNING"):
                self.ctl.ensure_playing()
        # The first getZones is the look before the wake; then the wait, 3 s per look.
        looks = [c for c in get.call_args_list if c.args[0].endswith("getZones")][1:]
        self.assertEqual(self.now - start, 3 + ra.WAKE_WAIT_S)
        # Each look may only take what is left of the wait: at +0, +3.5 and +7 s.
        self.assertEqual([c.args[1] for c in looks], [ra.WAKE_WAIT_S, 6.5, 3.0])
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

    def slow_wake_looks(self, look_s, times_out=lambda deadline_s: False):
        """Each getZones takes look_s; a look after the wake for whose time limit times_out
        is true takes all of it and times out."""
        real_get = ra.http_get

        def get(url, *args):
            woken = "leaveStandby" in self.rf.api_calls()
            if url.endswith("getZones") and woken and times_out(*args):
                self.now += args[0]
                raise socket.timeout("timed out")
            return real_get(url, *args)

        def slow_answer():
            self.now += look_s
        self.rf.on_get_zones = slow_answer
        return mock.patch.object(ra, "http_get", side_effect=get)

    def test_a_wake_look_running_into_the_deadline_still_takes_over(self):
        self.rf.wake_after_polls = 1000
        start = self.now
        # Looks at +0 and +3.5 s answer (standby); the one at +7 s times out at the deadline.
        with self.slow_wake_looks(3, times_out=lambda deadline_s: deadline_s < 3.5):
            with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
                self.ctl.ensure_playing()
        self.assertTrue(any("anyway" in m for m in logs.output), logs.output)
        self.assertEqual(self.now - start, 3 + ra.WAKE_WAIT_S)
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])
        self.assertEqual(self.discover.searches[-1], ZONE_UDN)  # the zone of the last answer
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())

    def test_a_wake_wait_in_which_no_look_answers_fails_the_takeover(self):
        self.rf.wake_after_polls = 1000
        with self.slow_wake_looks(0, times_out=lambda deadline_s: True):
            with self.assertRaises(socket.timeout):
                self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())

    def test_the_last_wake_sleep_never_passes_the_deadline(self):
        self.rf.wake_after_polls = 1000
        start = self.now
        with self.slow_wake_looks(4.7):
            with self.assertLogs("raumfeld_adapter", "WARNING"):
                self.ctl.ensure_playing()
        # Looks at +4.7 and +9.9 s (each 4.7 s); only 0.1 s is left for the last sleep.
        self.assertAlmostEqual(self.now - start, 4.7 + ra.WAKE_WAIT_S, places=9)
        self.assertAlmostEqual(self.sleeps[-1], 0.1, places=9)
        self.assertEqual(self.rf.soap_actions(), ["SetAVTransportURI", "Play"])

    def test_woken_room_active_without_its_zone_is_waited_for_not_reconnected(self):
        self.rf.rejoin_after_polls = 3  # ACTIVE at once, back in its zone a little later
        self.ctl.ensure_playing()
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())
        self.assertEqual(self.rf.api_calls(), ["getZones", "leaveStandby"] + ["getZones"] * 4)
        self.assertEqual(self.discover.searches[-1], ZONE_UDN)
        self.assertEqual(self.rf.transport_state, "PLAYING")

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

    def test_a_failing_cached_renderer_is_discovered_again_at_once(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()
        self.rf.fail_paths.add("AVTransport")
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.check()
        # The cached address failed, so the renderer was found again and asked once more.
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN, ZONE_UDN])
        self.rf.fail_paths.clear()
        self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN] + [ZONE_UDN] * 3)

    def test_renderer_state_names_the_state_and_flags_another_source(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()  # the zone renderer is known from now on
        self.rf.requests.clear()
        for state, uri, seen in (("PLAYING", STREAM_URL, "PLAYING"),
                                 # line-in reports PLAYING
                                 ("PLAYING", LINE_IN_URI, "PLAYING (another source)"),
                                 ("TRANSITIONING", STREAM_URL, "TRANSITIONING"),
                                 ("STOPPED", STREAM_URL, "STOPPED"),
                                 ("STOPPED", OTHER_URI, "STOPPED (another source)"),
                                 # nothing loaded is no source anyone chose
                                 ("STOPPED", "", "STOPPED (nothing loaded)"),
                                 ("PLAYING", "", "PLAYING (nothing loaded)")):
            with self.subTest(state=state, uri=uri):
                self.rf.transport_state, self.rf.current_uri = state, uri
                self.assertEqual(self.ctl.renderer_state(), seen)
        self.assertEqual(self.rf.api_calls(), [])  # the renderer only, no getZones
        self.assertEqual(set(self.rf.soap_actions()), {"GetTransportInfo", "GetMediaInfo"})

    def test_renderer_state_without_a_known_zone_raises(self):
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.renderer_state()

    def test_renderer_state_truncates_what_the_device_reports(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()
        self.rf.transport_state, self.rf.current_uri = "X" * 500, OTHER_URI
        self.assertEqual(self.ctl.renderer_state(), "X" * 64 + " (another source)")

    def test_retake_leaves_a_zone_playing_another_source_alone(self):
        for state in ("PLAYING", "TRANSITIONING"):
            with self.subTest(state=state):
                self.rf.requests.clear()
                self.rf.power = "ACTIVE"
                self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, state
                self.assertEqual(self.ctl.ensure_playing(retake=True), ra.OTHER_SOURCE)
                self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
                self.assertEqual(self.rf.api_calls(), ["getZones"])  # not woken either
                self.assertEqual(self.rf.current_uri, LINE_IN_URI)

    def test_renderer_state_tells_the_source_before_the_takeover_from_a_new_one(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.ctl.ensure_playing()  # the cast takes line-in over
        for uri, seen in ((LINE_IN_URI, "PLAYING (source before the takeover)"),
                          (OTHER_URI, "PLAYING (another source)"),
                          (STREAM_URL, "PLAYING")):
            with self.subTest(uri=uri):
                self.rf.current_uri, self.rf.transport_state = uri, "PLAYING"
                self.assertEqual(self.ctl.renderer_state(), seen)

    def test_after_a_takeover_from_standby_any_source_is_a_new_one(self):
        self.ctl.ensure_playing()  # nothing was playing before
        for uri, seen in ((LINE_IN_URI, "PLAYING (another source)"),
                          (OTHER_URI, "PLAYING (another source)"),
                          # an empty URI is nothing loaded, not a source someone chose
                          ("", "PLAYING (nothing loaded)")):
            with self.subTest(uri=uri):
                self.rf.current_uri, self.rf.transport_state = uri, "PLAYING"
                self.assertEqual(self.ctl.renderer_state(), seen)

    def test_retake_sets_up_a_renderer_playing_nothing_loaded(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = "", "PLAYING"
        self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_takeover_finding_our_stream_playing_forgets_an_older_source_before(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.ctl.ensure_playing()  # the cast takes line-in over, then ends
        self.assertEqual(self.ctl.ensure_playing(), ra.ALREADY_PLAYING)  # the next cast
        self.rf.current_uri = LINE_IN_URI  # someone switches to line-in now
        self.assertEqual(self.ctl.renderer_state(), "PLAYING (another source)")

    def test_a_retake_keeps_the_source_before_the_first_takeover(self):
        for retake_finds in ("our stream stopped", "standby"):
            with self.subTest(retake_finds=retake_finds):
                self.rf.power = "ACTIVE"
                self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
                self.rf.drop_takeovers = 1
                self.ctl.ensure_playing()  # dropped: still on line-in
                if retake_finds == "standby":
                    self.rf.power = "MANUAL_STANDBY"
                else:
                    self.rf.current_uri, self.rf.transport_state = STREAM_URL, "STOPPED"
                self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
                # The speaker drops back to line-in: still the source before the takeover.
                self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
                self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
                self.assertEqual(self.rf.current_uri, STREAM_URL)

    def test_retake_takes_over_the_source_before_the_takeover_again(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.rf.drop_takeovers = 1
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.current_uri, LINE_IN_URI)  # dropped
        self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_retake_leaves_a_source_other_than_the_one_before_the_takeover_alone(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.rf.drop_takeovers = 1
        self.ctl.ensure_playing()
        self.rf.current_uri = OTHER_URI  # someone starts a radio station
        self.assertEqual(self.ctl.ensure_playing(retake=True), ra.OTHER_SOURCE)
        self.assertEqual(self.rf.current_uri, OTHER_URI)

    def test_retake_sets_up_a_renderer_that_lost_our_stream(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = "", "NO_MEDIA_PRESENT"
        self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
        self.assertEqual(self.rf.soap_actions()[-2:], ["SetAVTransportURI", "Play"])
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_retake_wakes_a_room_back_in_standby(self):
        self.assertEqual(self.ctl.ensure_playing(retake=True), ra.TAKE_OVER)
        self.assertIn("leaveStandby", self.rf.api_calls())
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_renderer_transitioning_on_our_stream_is_left_alone(self):
        for retake in (False, True):
            with self.subTest(retake=retake):
                self.rf.requests.clear()
                self.rf.power = "ACTIVE"
                self.rf.current_uri, self.rf.transport_state = STREAM_URL, "TRANSITIONING"
                self.assertEqual(self.ctl.ensure_playing(retake=retake), ra.ALREADY_PLAYING)
                self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])

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

    # -- discovery is cached: the host by its API address, the zone renderer by zone UDN

    def test_takeover_reuses_the_cached_discovery(self):
        self.ctl.ensure_playing()
        self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])

    def test_warm_up_finds_host_and_renderer_so_a_cast_needs_no_discovery(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.ctl.warm_up()  # at adapter start; the room is in standby
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertEqual(self.rf.soap_actions(), [])  # nothing sent to a sleeping renderer
        self.assertEqual(self.rf.api_calls(), ["getZones"])
        self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])  # no SSDP
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_warm_up_that_finds_nothing_is_logged_not_raised(self):
        self.discover.speaker_present = False
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.ctl.warm_up()
        self.discover.speaker_present = True
        self.ctl.ensure_playing()  # the first cast looks again
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_takeover_reports_its_wake_wait_and_discoveries(self):
        self.rf.wake_after_polls = 2
        self.ctl.ensure_playing()
        self.assertEqual(self.ctl.last_takeover,
                         ra.TakeoverStats(wake_s=2 * ra.WAKE_POLL_S, lookups=2))
        self.rf.power, self.rf.transport_state = "MANUAL_STANDBY", "STOPPED"
        self.ctl.ensure_playing()  # everything cached now
        self.assertEqual(self.ctl.last_takeover,
                         ra.TakeoverStats(wake_s=2 * ra.WAKE_POLL_S, lookups=0))

    def test_a_moved_renderer_is_discovered_again_and_the_takeover_goes_on(self):
        self.rf.power = "ACTIVE"
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.ctl.warm_up()
        self.rf.control_url = "AVTransport/moved"  # the cached control URL now fails
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN, ZONE_UDN])
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_a_new_zone_is_discovered_the_old_ones_renderer_is_not_used(self):
        self.ctl.ensure_playing()
        self.rf.zone_udn = NEW_ZONE_UDN
        self.rf.transport_state = "STOPPED"
        self.ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN, NEW_ZONE_UDN])

    def test_a_failing_host_is_found_again_and_asked_once_more(self):
        self.ctl.ensure_playing()
        real_get, failed = ra.http_get, []

        def get(url, *args):
            if url.endswith("getZones") and not failed:
                failed.append(url)
                raise ConnectionRefusedError("host moved")
            return real_get(url, *args)

        with mock.patch.object(ra, "http_get", get):
            self.assertEqual(self.ctl.ensure_playing(), ra.ALREADY_PLAYING)
        self.assertEqual(failed, [self.rf.base + "/getZones"])
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN, SPEAKER_UDN])

    def test_a_host_that_does_not_know_our_room_is_an_error_never_a_takeover(self):
        self.rf.room_listed = False  # a wrong host
        for action in (self.ctl.ensure_playing, self.ctl.check):
            with self.subTest(action=action.__name__), \
                    self.assertRaisesRegex(ra.RaumfeldError, "room"):
                action()
        self.assertEqual(set(self.rf.api_calls()), {"getZones"})  # no wake, no new zone
        self.assertEqual(self.rf.soap_actions(), [])

    def test_a_renderer_description_of_another_device_is_refused(self):
        self.rf.power = "ACTIVE"
        self.rf.description_udn = NEW_ZONE_UDN
        with self.assertRaisesRegex(ra.RaumfeldError, "UDN"):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])

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


class ControllerWithHostTest(unittest.TestCase):
    """RAUMFELD_HOST: the host API is reached directly, the zone renderer found by a
    unicast M-SEARCH to the host; UDNs are still verified."""

    HOST = "raumfeld-host.invalid"

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.ssdp = FakeSsdpResponder(self.rf)
        self.addCleanup(self.ssdp.close)
        self.resolved = []
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN, "RAUMFELD_HOST": self.HOST,
                                     "STREAM_URL": STREAM_URL})
        self.locator = ra.HostLocator(config.raumfeld_host, resolve=self.resolve,
                                      ssdp_port=self.ssdp.port)
        self.ctl = ra.Controller(config, self.locator, api_port=self.rf.port,
                                 sleep=lambda s: None)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    def resolve(self, name):
        self.resolved.append(name)
        return "127.0.0.1"

    def test_cast_reaches_the_host_directly_and_finds_the_renderer_by_unicast(self):
        self.ctl.ensure_playing()
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.resolved, [self.HOST])
        self.assertEqual(len(self.ssdp.received), 1)
        search = self.ssdp.received[0]
        self.assertTrue(search.startswith("M-SEARCH * HTTP/1.1\r\n"))
        self.assertIn("HOST: 127.0.0.1:%d\r\n" % self.ssdp.port, search)
        self.assertIn("ST: %s\r\n" % ZONE_UDN, search)
        self.rf.transport_state = "STOPPED"
        self.ctl.ensure_playing()  # cached: no DNS, no SSDP
        self.assertEqual(self.resolved, [self.HOST])
        self.assertEqual(len(self.ssdp.received), 1)

    def test_warm_up_resolves_and_finds_the_renderer(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.ctl.warm_up()
        self.assertEqual((self.resolved, len(self.ssdp.received)), ([self.HOST], 1))

    def test_the_name_is_resolved_again_after_a_failure(self):
        self.ctl.ensure_playing()
        real_get, failed = ra.http_get, []

        def get(url, *args):
            if url.endswith("getZones") and not failed:
                failed.append(url)
                raise ConnectionRefusedError("host moved")
            return real_get(url, *args)

        with mock.patch.object(ra, "http_get", get):
            self.ctl.check()
        self.assertEqual(self.resolved, [self.HOST, self.HOST])

    def test_a_wrong_host_is_an_error_never_a_takeover(self):
        self.ctl.ensure_playing()
        self.rf.room_listed = False  # the name now points at another Raumfeld host
        self.rf.requests.clear()
        with self.assertRaisesRegex(ra.RaumfeldError, "room"):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.api_calls(), ["getZones", "getZones"])  # asked again, fresh
        self.assertEqual(self.resolved, [self.HOST] * 2)  # resolved again for the retry
        self.assertEqual(self.rf.soap_actions(), [])

    def test_a_renderer_of_another_device_is_refused(self):
        self.rf.power = "ACTIVE"
        self.rf.description_udn = NEW_ZONE_UDN
        with self.assertRaisesRegex(ra.RaumfeldError, "UDN"):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])

    def test_an_answer_from_another_address_is_refused(self):
        locator = ra.HostLocator(self.HOST, resolve=self.resolve,
                                 search=lambda st, addr, timeout: ("127.0.0.9", "http://x/"))
        with self.assertRaisesRegex(ra.RaumfeldError, "127.0.0.9"):
            locator.renderer(ZONE_UDN)

    def test_no_unicast_answer_is_device_not_found(self):
        self.rf.power = "ACTIVE"
        self.ssdp.silent = True
        with mock.patch.object(ra, "UNICAST_SEARCH_S", 0.3), \
                self.assertRaises(ra.DeviceNotFound):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])


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
        self.ctl = ra.Controller(config, ra.SsdpLocator(self.discover, SPEAKER_UDN),
                                 api_port=self.rf.port, sleep=lambda s: None)

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
