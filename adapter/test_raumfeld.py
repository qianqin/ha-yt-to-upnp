"""Feature: Raumfeld follows the cast -- Controller against a fake Raumfeld host."""

import errno
import json
import logging
import os
import socket
import stat
import tempfile
import threading
import time
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, NEW_ZONE_UDN, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN,
                   FakeDiscovery, FakeRaumfeld, FakeSsdpResponder)
from test_logic import no_logs
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

    def unreachable(self, error, times=1):
        """soap_call fails with `error` (no answer from the device) for the next `times`
        calls, then reaches the fake again."""
        real, failures = ra.soap_call, []

        def soap_call(url, action, args):
            if len(failures) < times:
                failures.append(url)
                raise error
            return real(url, action, args)

        return mock.patch.object(ra, "soap_call", soap_call)

    def test_a_renderer_unreachable_at_its_cached_address_is_discovered_again_at_once(self):
        self.rf.power = "ACTIVE"
        self.ctl.check()
        errors = (ConnectionRefusedError("renderer moved"), socket.timeout("timed out"),
                  ra.RequestTimeout("took longer than the request deadline"))
        for n, error in enumerate(errors, 1):
            with self.subTest(error=error):
                with self.unreachable(error):
                    self.ctl.check()  # answered after the new lookup
                # The cached address failed, so the renderer was found again and asked once more.
                self.assertEqual(self.discover.searches, [SPEAKER_UDN] + [ZONE_UDN] * (n + 1))
        with self.unreachable(ConnectionRefusedError("gone"), times=2), \
                self.assertRaises(ConnectionRefusedError):
            self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN] + [ZONE_UDN] * 5)

    def test_a_soap_fault_from_the_cached_renderer_is_no_reason_to_look_for_it_again(self):
        # The device answered (UPnP fault, HTTP 500): it is the right one, just unwilling.
        self.rf.power = "ACTIVE"
        self.ctl.check()
        self.rf.fail_paths.add("AVTransport")
        self.rf.requests.clear()
        with self.assertRaisesRegex(ra.RaumfeldError, "HTTP 500"):
            self.ctl.check()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo"])  # not sent again
        self.rf.fail_paths.clear()
        self.ctl.check()  # the cached address is kept
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])

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
        with self.unreachable(ConnectionRefusedError("renderer moved")), \
                self.assertLogs("raumfeld_adapter", "INFO"):
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
    """RAUMFELD_HOST: the host API is reached directly; the zone renderer is found by a
    multicast M-SEARCH (the Raumfeld doesn't answer unicast ones) whose answers count only
    from the host's address. UDNs are still verified. The fake responder stands in for the
    multicast group: no real multicast in tests."""

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
                                      group=("127.0.0.1", self.ssdp.port))
        self.ctl = ra.Controller(config, self.locator, api_port=self.rf.port,
                                 sleep=lambda s: None)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    address = "127.0.0.1"  # what the host name resolves to

    def resolve(self, name):
        self.resolved.append(name)
        return self.address

    def test_cast_reaches_the_host_directly_and_finds_the_renderer_by_multicast(self):
        started = time.monotonic()
        self.ctl.ensure_playing()
        self.assertLess(time.monotonic() - started, 0.9)  # the first answer, not the timeout
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.resolved, [self.HOST])
        self.assertEqual(len(self.ssdp.received), 1)
        search = self.ssdp.received[0]
        self.assertTrue(search.startswith("M-SEARCH * HTTP/1.1\r\n"))
        self.assertIn("HOST: 239.255.255.250:1900\r\n", search)
        self.assertIn("MX: 1\r\n", search)
        self.assertIn("ST: %s\r\n" % ZONE_UDN, search)
        self.rf.transport_state = "STOPPED"
        self.ctl.ensure_playing()  # cached: no DNS, no SSDP
        self.assertEqual(self.resolved, [self.HOST])
        self.assertEqual(len(self.ssdp.received), 1)

    def test_takeover_counts_resolving_the_name_and_the_renderer_search_as_lookups(self):
        self.ctl.ensure_playing()
        self.assertEqual(self.ctl.last_takeover.lookups, 2)

    def test_an_ipv4_address_as_raumfeld_host_is_no_lookup(self):
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN, "RAUMFELD_HOST": "127.0.0.1",
                                     "STREAM_URL": STREAM_URL})
        locator = ra.HostLocator(config.raumfeld_host, group=("127.0.0.1", self.ssdp.port))
        ctl = ra.Controller(config, locator, api_port=self.rf.port, sleep=lambda s: None)
        ctl.ensure_playing()
        self.assertEqual(ctl.last_takeover.lookups, 1)  # the renderer search only

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

    def test_answers_from_another_address_are_ignored(self):
        # Another Raumfeld host (or anything else on the LAN) answering for the zone UDN.
        self.address = "192.0.2.9"  # the configured host; the fake answers from 127.0.0.1
        with mock.patch.object(ra, "HOST_SEARCH_S", 1.2), \
                self.assertLogs("raumfeld_adapter", "WARNING") as logs, \
                self.assertRaises(ra.DeviceNotFound):
            self.locator.renderer(ZONE_UDN)
        self.assertEqual(len(self.ssdp.received), 2)  # re-sent after a second, then gave up
        self.assertEqual(len(logs.output), 1, logs.output)  # names both addresses, once
        self.assertIn("from 127.0.0.1", logs.output[0])
        self.assertIn("only 192.0.2.9", logs.output[0])

    def test_no_answer_is_device_not_found(self):
        self.rf.power = "ACTIVE"
        self.ssdp.silent = True
        with mock.patch.object(ra, "HOST_SEARCH_S", 0.3), \
                self.assertRaises(ra.DeviceNotFound):
            self.ctl.ensure_playing()
        self.assertEqual(self.rf.soap_actions(), [])


class StoredRendererTest(unittest.TestCase):
    """ADAPTER_STATE_DIR: the zone renderer's location survives adapter restarts. At start it
    is used first, without a search, when its description names the zone; otherwise the
    search runs and the file is updated."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.discover = FakeDiscovery(self.rf)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        self.path = os.path.join(self.dir, ra.STATE_FILE_NAME)
        self.config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                          "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                          "STREAM_URL": STREAM_URL,
                                          "ADAPTER_STATE_DIR": self.dir})

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    def controller(self, locator=None):
        # The fake host serves on 127.0.0.1, which only tests accept as a renderer host.
        locator = locator or ra.SsdpLocator(self.discover, SPEAKER_UDN, allow_loopback=True)
        return ra.Controller(self.config, locator,
                             api_port=self.rf.port, sleep=lambda s: None,
                             state=ra.StateFile(self.path))

    def store(self, entry):
        with open(self.path, "w") as f:
            f.write(entry if isinstance(entry, str) else json.dumps(entry))

    def stored(self):
        with open(self.path) as f:
            return json.load(f)

    def entry(self, location=None, zone=ZONE_UDN, host="127.0.0.1"):
        return {"zone_udn": zone, "host": host, "location": location or self.rf.base + "/zone.xml"}

    def warm_up(self, ctl, level="INFO"):
        with self.assertLogs("raumfeld_adapter", level) as logs:
            ctl.warm_up()
        return logs.output

    def test_a_found_renderer_is_stored_atomically_and_privately(self):
        self.warm_up(self.controller())
        self.assertEqual(self.stored(), self.entry())
        self.assertEqual(stat.S_IMODE(os.stat(self.path).st_mode), 0o600)
        self.assertEqual(os.listdir(self.dir), [ra.STATE_FILE_NAME])  # no temporary left

    def test_a_stored_renderer_is_used_at_start_without_a_search(self):
        self.store(self.entry())
        ctl = self.controller()
        self.warm_up(ctl)
        self.assertEqual(self.discover.searches, [SPEAKER_UDN])  # the host only, no zone search
        ctl.ensure_playing()
        self.assertEqual(self.discover.searches, [SPEAKER_UDN])
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_a_stored_renderer_that_does_not_answer_falls_back_to_the_search(self):
        dead = socket.socket()
        dead.bind(("127.0.0.1", 0))
        port = dead.getsockname()[1]
        dead.close()  # nothing listens there now
        self.store(self.entry("http://127.0.0.1:%d/zone.xml" % port))
        self.warm_up(self.controller())
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertEqual(self.stored(), self.entry())  # updated

    def test_a_stored_location_of_another_device_falls_back_to_the_search(self):
        other = FakeRaumfeld()
        self.addCleanup(other.close)
        other.zone_udn = NEW_ZONE_UDN  # its description names another UDN
        self.store(self.entry(other.base + "/zone.xml"))
        self.warm_up(self.controller())
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertEqual(self.stored(), self.entry())
        self.assertEqual(other.soap_actions(), [])

    def test_a_stored_renderer_of_another_zone_is_not_used(self):
        self.store(self.entry(zone=NEW_ZONE_UDN))
        self.warm_up(self.controller())
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertEqual(self.stored(), self.entry())

    def test_an_unusable_file_is_ignored_with_a_warning(self):
        for content in ("not json", "[]", json.dumps({"zone_udn": ZONE_UDN}),
                        json.dumps(dict(self.entry(), zone_udn="uuid:x\r\nST: ssdp:all")),
                        json.dumps(dict(self.entry(), host=5)),
                        json.dumps(self.entry("file:///etc/passwd")),
                        # The location must be on the stored host, like a discovery's.
                        json.dumps(self.entry("http://192.0.2.1:%d/zone.xml" % self.rf.port)),
                        "x" * (ra.STATE_MAX_BYTES + 1),
                        # A valid entry, only too large ...
                        json.dumps(self.entry(self.rf.base + "/zone.xml?" + "a" * ra.STATE_MAX_BYTES)),
                        # ... also where what fits in the limit would parse on its own.
                        json.dumps(self.entry()) + " " * ra.STATE_MAX_BYTES,
                        # Nested deeper than the JSON parser recurses, yet within the size.
                        "[" * 2000 + "]" * 2000):
            with self.subTest(content=content[:40]):
                self.discover.searches.clear()
                self.store(content)
                output = self.warm_up(self.controller(), "WARNING")
                self.assertTrue(any("state" in m for m in output), output)
                self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
                self.assertEqual(self.stored(), self.entry())  # replaced by what was found

    def stored_fetched(self):
        return [p for m, p, a, b in self.rf.requests if p.endswith("?stored")]

    def test_in_ssdp_mode_a_stored_loopback_host_is_refused_by_default(self):
        self.store(self.entry(self.rf.base + "/zone.xml?stored"))  # reachable, our zone
        output = self.warm_up(self.controller(ra.SsdpLocator(self.discover, SPEAKER_UDN)))
        self.assertTrue(any("state" in m for m in output), output)
        self.assertEqual(self.stored_fetched(), [])
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])

    def test_a_state_file_descriptor_is_closed_if_it_cannot_be_wrapped(self):
        self.store(self.entry())
        real_open, opened = os.open, []

        def open_(path, flags, *args):
            fd = real_open(path, flags, *args)
            opened.append(fd)
            return fd

        def fdopen(fd, *args, **kwargs):
            raise OSError(errno.EMFILE, "Too many open files")

        with mock.patch.object(ra.os, "open", open_), mock.patch.object(ra.os, "fdopen", fdopen):
            self.assertIsNone(self.state_file_load())
        self.assertEqual(len(opened), 1)
        with self.assertRaises(OSError) as raised:
            os.fstat(opened[0])
        self.assertEqual(raised.exception.errno, errno.EBADF)

    def state_file_load(self):
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            return ra.StateFile(self.path).load()

    def test_in_ssdp_mode_a_stored_host_off_the_lan_is_never_fetched(self):
        # The first is reachable and describes our zone; only its host is a name, which no
        # SSDP sender is. The second is an address outside the LAN.
        for host in ("localhost", "203.0.113.9"):
            with self.subTest(host=host):
                self.rf.requests.clear()
                self.discover.searches.clear()
                self.store(self.entry("http://%s:%d/zone.xml?stored" % (host, self.rf.port),
                                      host=host))
                output = self.warm_up(self.controller())
                self.assertTrue(any("state" in m for m in output), output)
                self.assertEqual(self.stored_fetched(), [])
                self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
                self.assertEqual(self.stored(), self.entry())

    def test_a_symlinked_state_file_is_ignored_with_a_warning(self):
        target = os.path.join(self.dir, "elsewhere.json")
        with open(target, "w") as f:
            json.dump(self.entry(), f)
        os.symlink(target, self.path)
        output = self.warm_up(self.controller(), "WARNING")
        self.assertTrue(any("state" in m for m in output), output)
        self.assertEqual(self.discover.searches, [SPEAKER_UDN, ZONE_UDN])
        self.assertFalse(os.path.islink(self.path))  # replaced by a file of our own
        self.assertEqual(self.stored(), self.entry())

    def test_a_state_file_that_is_no_regular_file_is_ignored_without_blocking(self):
        os.mkfifo(self.path)  # opening a FIFO for reading would block until a writer comes
        ctl, output = self.controller(), []
        runner = threading.Thread(target=lambda: output.extend(self.warm_up(ctl, "WARNING")),
                                  daemon=True)
        runner.start()
        runner.join(5)
        if runner.is_alive():
            unblock_fifo(self.path)
            runner.join(2)
            self.fail("the state blocked on a FIFO")
        self.assertTrue(any("regular" in m for m in output), output)
        self.assertEqual(self.stored(), self.entry())

    def test_a_state_dir_writable_by_others_is_warned_about(self):
        self.store(self.entry())
        for mode in (0o770, 0o707):
            with self.subTest(mode=oct(mode)):
                os.chmod(self.dir, mode)
                self.addCleanup(os.chmod, self.dir, 0o700)
                output = self.warm_up(self.controller(), "WARNING")
                self.assertTrue(any("writable" in m and self.dir in m for m in output), output)

    def test_a_private_state_dir_is_not_warned_about(self):
        self.store(self.entry())
        os.chmod(self.dir, 0o755)
        with no_logs(self, "raumfeld_adapter", logging.WARNING):
            self.controller().warm_up()

    def test_a_failed_write_leaves_the_old_file_intact_and_no_temporary(self):
        old = self.entry(zone=NEW_ZONE_UDN)  # of another zone: the search runs and saves
        self.store(old)
        real_dump = json.dump

        def dump(obj, f):
            f.write(json.dumps(obj)[:10])  # part of the new entry is written ...
            raise OSError(errno.ENOSPC, "No space left on device")  # ... then the disk is full

        with mock.patch.object(ra.json, "dump", dump):
            output = self.warm_up(self.controller(), "WARNING")
        self.assertTrue(any("could not write" in m for m in output), output)
        self.assertEqual(self.stored(), old)
        self.assertEqual(os.listdir(self.dir), [ra.STATE_FILE_NAME])
        self.assertIs(json.dump, real_dump)

    def test_the_new_file_is_renamed_over_the_old_one_from_the_same_directory(self):
        real_replace, replaced = os.replace, []

        def replace(src, dst):
            replaced.append((src, dst))
            return real_replace(src, dst)

        with mock.patch.object(ra.os, "replace", replace):
            self.warm_up(self.controller())
        self.assertEqual(len(replaced), 1)
        src, dst = replaced[0]
        self.assertEqual((os.path.dirname(src), dst), (self.dir, self.path))

    def test_an_unwritable_directory_is_logged_and_the_adapter_goes_on(self):
        self.path = os.path.join(self.dir, "missing", ra.STATE_FILE_NAME)
        ctl = self.controller()
        output = self.warm_up(ctl, "WARNING")
        self.assertTrue(any("state" in m for m in output), output)
        ctl.ensure_playing()
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def host_controller(self, searched):
        locator = ra.HostLocator("raumfeld-host.invalid", resolve=lambda name: "127.0.0.1",
                                 group=("127.0.0.1", 9))

        def renderer(zone):
            searched.append(zone)
            return "127.0.0.1", self.rf.base + "/zone.xml"

        locator.renderer = renderer
        return ra.Controller(self.config, locator, api_port=self.rf.port, sleep=lambda s: None,
                             state=ra.StateFile(self.path))

    def test_with_raumfeld_host_a_stored_renderer_must_be_on_that_host(self):
        # Stored when the host had another address (or before RAUMFELD_HOST was set).
        self.store(self.entry("http://192.0.2.9:%d/zone.xml" % self.rf.port, host="192.0.2.9"))
        searched = []
        output = self.warm_up(self.host_controller(searched))
        self.assertTrue(any("state" in m for m in output), output)
        self.assertEqual(searched, [ZONE_UDN])
        self.assertEqual(self.stored(), self.entry())

    def test_with_raumfeld_host_a_stored_renderer_on_another_host_is_never_fetched(self):
        # Reachable and describing our zone; only its host is not RAUMFELD_HOST's address.
        self.store(self.entry("http://localhost:%d/zone.xml?stored" % self.rf.port,
                              host="localhost"))
        searched = []
        output = self.warm_up(self.host_controller(searched))
        self.assertTrue(any("state" in m for m in output), output)
        self.assertEqual(self.stored_fetched(), [])
        self.assertEqual(searched, [ZONE_UDN])
        self.assertEqual(self.stored(), self.entry())

    def test_with_raumfeld_host_a_stored_renderer_on_it_is_used_without_a_search(self):
        self.store(self.entry())
        searched = []
        ctl = self.host_controller(searched)
        self.warm_up(ctl)
        ctl.ensure_playing()
        self.assertEqual(searched, [])
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_warm_up_logs_the_renderer_control_url_quoted(self):
        output = self.warm_up(self.controller())
        self.assertTrue(any("'%s/AVTransport/ctrl'" % self.rf.base in m for m in output), output)

    def test_without_a_state_dir_nothing_is_written(self):
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        self.assertIsNone(config.state_dir)
        ctl = ra.Controller(config, ra.SsdpLocator(self.discover, SPEAKER_UDN),
                            api_port=self.rf.port, sleep=lambda s: None)
        self.warm_up(ctl)
        self.assertEqual(os.listdir(self.dir), [])


def unblock_fifo(path):
    """Open the other end of a FIFO that a regression blocks on (reading or writing)."""
    for flags in (os.O_RDONLY | os.O_NONBLOCK, os.O_WRONLY | os.O_NONBLOCK):
        try:
            os.close(os.open(path, flags))
        except OSError:
            pass  # no reader for a non-blocking writer: nothing waits on that end


class LocatorAcceptsTest(unittest.TestCase):
    """Which host a stored renderer location may be on."""

    def test_with_raumfeld_host_only_its_address(self):
        locator = ra.HostLocator("raumfeld-host.invalid", resolve=lambda name: "192.0.2.5")
        self.assertTrue(locator.accepts("192.0.2.5"))
        for host in ("192.0.2.6", "raumfeld-host.invalid", "localhost", ""):
            with self.subTest(host=host):
                self.assertFalse(locator.accepts(host))

    def test_in_ssdp_mode_an_ipv4_address_on_the_lan(self):
        locator = ra.SsdpLocator(lambda st: None, SPEAKER_UDN)
        # Generic examples of the private and link-local ranges; no real network.
        for host in ("10.0.0.2", "172.16.0.9", "169.254.3.4"):
            with self.subTest(host=host):
                self.assertTrue(locator.accepts(host))
        # Off the LAN: documentation and other public addresses, loopback (a stored
        # 127.0.0.1 would point at a local service), "this network", broadcast, multicast,
        # names, IPv6.
        for host in ("203.0.113.9", "198.51.100.7", "192.0.2.1", "127.0.0.1", "0.0.0.0",
                     "0.1.2.3", "255.255.255.255", "239.255.255.250", "localhost",
                     "raumfeld.example", "::1", "10.0.0.2 ", ""):
            with self.subTest(host=host):
                self.assertFalse(locator.accepts(host))

    def test_tests_may_allow_loopback_for_the_fake_host(self):
        locator = ra.SsdpLocator(lambda st: None, SPEAKER_UDN, allow_loopback=True)
        self.assertTrue(locator.accepts("127.0.0.1"))
        self.assertTrue(locator.accepts("10.0.0.2"))
        self.assertFalse(locator.accepts("203.0.113.9"))


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


class RendererPauseTest(unittest.TestCase):
    """Scenario: a user pause is followed to the zone renderer of the last look (Pause), its
    resume plays it again (Play); the URI is left as it is, nothing else is sent."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        self.ctl = ra.Controller(config, ra.SsdpLocator(FakeDiscovery(self.rf), SPEAKER_UDN),
                                 api_port=self.rf.port, sleep=lambda s: None)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    def test_pause_then_play_on_the_checked_zone(self):
        self.rf.power, self.rf.current_uri, self.rf.transport_state = "ACTIVE", STREAM_URL, "PLAYING"
        self.assertEqual(self.ctl.check(), ra.OURS)
        self.rf.requests.clear()
        self.ctl.pause_renderer()
        self.assertEqual(self.rf.soap_actions(), ["Pause"])
        self.assertEqual(self.ctl.renderer_state(), "PAUSED_PLAYBACK")
        self.rf.requests.clear()
        self.ctl.play_renderer()
        self.assertEqual(self.rf.soap_actions(), ["Play"])
        self.assertIn("<Speed>1</Speed>", self.rf.soap_body("Play"))
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.api_calls(), [])  # no wake, no zone change

    def test_pause_and_play_without_a_checked_zone_raise(self):
        for call in (self.ctl.pause_renderer, self.ctl.play_renderer):
            with self.assertRaises(ra.RaumfeldError):
                call()
        self.assertEqual(self.rf.requests, [])


class RendererVolumeTest(unittest.TestCase):
    """Scenario: the speaker's volume is the zone renderer's RenderingControl volume (Master
    channel) on the zone of the last look."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        self.ctl = ra.Controller(config, ra.SsdpLocator(FakeDiscovery(self.rf), SPEAKER_UDN),
                                 api_port=self.rf.port, sleep=lambda s: None)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    def checked(self):
        self.rf.power, self.rf.current_uri, self.rf.transport_state = "ACTIVE", STREAM_URL, "PLAYING"
        self.assertEqual(self.ctl.check(), ra.OURS)
        self.rf.requests.clear()

    def test_get_and_set_volume_on_the_checked_zone(self):
        self.checked()
        self.rf.volume = 33
        self.assertEqual(self.ctl.get_volume(), 33)
        self.ctl.set_volume(25)
        self.assertEqual(self.rf.volume, 25)
        self.assertEqual(self.rf.volume_actions(), ["GetRoomVolume", "SetRoomVolume 25"])
        self.assertEqual(self.rf.soap_actions(), [])  # nothing on AVTransport
        self.assertEqual(self.rf.api_calls(), [])

    def test_a_volume_out_of_range_is_refused_before_it_is_sent(self):
        self.checked()
        for volume in (-1, 101, 2.5, True):
            with self.subTest(volume=volume), self.assertRaises(ValueError):
                self.ctl.set_volume(volume)
        self.assertEqual(self.rf.requests, [])

    def test_a_device_volume_that_is_no_volume_is_an_error(self):
        self.checked()
        for reported in ("", "loud", "101", "-3"):
            with self.subTest(reported=reported):
                self.rf.volume = reported
                with self.assertRaises(ra.RaumfeldError):
                    self.ctl.get_volume()

    def test_volume_without_a_checked_zone_raises(self):
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.get_volume()
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.set_volume(10)
        self.assertEqual(self.rf.requests, [])

    def test_a_renderer_without_rendering_control_has_no_volume_but_plays(self):
        self.rf.rendering_control = False
        self.checked()
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.get_volume()
        self.assertEqual(self.rf.requests, [])



class RendererPositionTest(unittest.TestCase):
    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        self.ctl = ra.Controller(config, ra.SsdpLocator(FakeDiscovery(self.rf), SPEAKER_UDN),
                                 api_port=self.rf.port, sleep=lambda s: None)

    def test_position_is_the_renderers_reltime(self):
        self.rf.power, self.rf.current_uri, self.rf.transport_state = "ACTIVE", STREAM_URL, "PLAYING"
        self.ctl.check()
        self.rf.rel_time = "0:03:20"
        self.assertEqual(self.ctl.position(), 200)
        self.rf.rel_time = "NOT_IMPLEMENTED"
        self.assertIsNone(self.ctl.position())

    def test_position_without_a_checked_zone_raises(self):
        with self.assertRaises(ra.RaumfeldError):
            self.ctl.position()



class RenderingControlOnAnotherHostTest(unittest.TestCase):
    def test_a_rendering_control_url_on_another_host_is_ignored(self):
        rf = FakeRaumfeld()
        self.addCleanup(rf.close)
        rf.rendering_url = "http://192.0.2.99/RenderingControl/ctrl"
        config = ra.Config.from_env({"RAUMFELD_ROOM_UDN": ROOM_UDN,
                                     "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
                                     "STREAM_URL": STREAM_URL})
        ctl = ra.Controller(config, ra.SsdpLocator(FakeDiscovery(rf), SPEAKER_UDN),
                            api_port=rf.port, sleep=lambda s: None)
        rf.power, rf.current_uri, rf.transport_state = "ACTIVE", STREAM_URL, "PLAYING"
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.assertEqual(ctl.check(), ra.OURS)  # the transport still works
        self.assertFalse(ctl.has_volume())
        with self.assertRaises(ra.RaumfeldError):
            ctl.get_volume()


if __name__ == "__main__":
    unittest.main()
