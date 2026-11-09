"""Feature: the speaker's own controls win -- turning the Raumfeld room off (by button or
by its own auto power-off) or switching it to another source ends the cast, and the
adapter stays away until the user resumes from the phone.

Real Follower + Controller against the fake Raumfeld host; MPD events are fed by hand.
"""

import time
import unittest

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN, FakeDiscovery,
                   FakeMpd, FakeRaumfeld, run_bounded)
from test_logic import GATE_LOOKS, FakeClock

POLL_S = 5
RADIO_URI = "http://radio.invalid/live.mp3"


def make_controller(rf, discover):
    config = ra.Config.from_env({
        "RAUMFELD_ROOM_UDN": ROOM_UDN,
        "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
        "STREAM_URL": STREAM_URL,
    })
    return ra.Controller(config, ra.SsdpLocator(discover, SPEAKER_UDN), api_port=rf.port,
                         sleep=lambda s: None)


def assert_speaker_left_alone(test, rf, restarts=0):
    """In every scenario: no volume, and the adapter never stops or powers down the room
    itself (the Raumfeld's own auto power-off does that); a Stop only as part of the
    expected number of renderer restarts (Stop, Play)."""
    for method, path, action, body in rf.requests:
        test.assertNotIn("RenderingControl", path)
        test.assertNotIn("Volume", path + (action or "") + body)
        test.assertNotIn("Standby", path.replace("leaveStandby", ""))
    test.assertEqual(rf.soap_actions().count("Stop"), restarts)


class SpeakerYieldTest(unittest.TestCase):
    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.discover = FakeDiscovery(self.rf)
        self.clock = FakeClock()
        self.follower = ra.Follower(make_controller(self.rf, self.discover),
                                    poll_seconds=POLL_S, clock=self.clock,
                                    start_gate_seconds=0)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    # -- helpers

    def mpd(self, *states):
        for state in states:
            self.follower.on_mpd_state(state)

    def poll(self):
        self.clock.now += POLL_S
        self.follower.tick()

    def requests(self):
        return self.follower.take_mpd_requests()

    def released_while_playing(self):
        # pause 1 first, so the stream stops even for a receiver that ignores the message
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])

    def repaused(self):
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])  # the release went out once

    def nothing_requested(self):
        self.assertEqual(self.requests(), [])

    def cast_is_playing(self):
        self.mpd("play")
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(self.rf.power, "ACTIVE")
        self.nothing_requested()
        self.rf.requests.clear()

    def taken_over(self):
        return "SetAVTransportURI" in self.rf.soap_actions() or "leaveStandby" in self.rf.api_calls()

    # -- taken away while playing

    def test_room_switched_to_standby_pauses_and_releases_and_track_change_does_not_wake_it(self):
        self.cast_is_playing()
        self.rf.power = "MANUAL_STANDBY"  # the speaker's own power button
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.poll()
        self.released_while_playing()
        self.assertTrue(any("standby" in m for m in logs.output), logs.output)
        # A song change that slipped in before MPD paused must not wake the room.
        self.mpd("stop", "play")
        self.assertFalse(self.taken_over())
        self.assertEqual(self.rf.power, "MANUAL_STANDBY")
        self.repaused()  # MPD is paused again so the phone shows it

    def test_automatic_standby_while_playing_counts_as_taken_away(self):
        self.cast_is_playing()
        self.rf.power = "AUTOMATIC_STANDBY"
        self.poll()
        self.released_while_playing()

    def test_speaker_turned_off_just_before_a_track_change_is_not_woken(self):
        self.cast_is_playing()
        self.rf.power = "MANUAL_STANDBY"
        self.mpd("stop", "play")  # before the next poll
        self.assertFalse(self.taken_over())
        self.assertEqual(self.rf.power, "MANUAL_STANDBY")
        self.released_while_playing()

    def test_zone_switched_to_line_in_pauses_and_releases_and_is_not_taken_over(self):
        self.cast_is_playing()
        self.rf.current_uri = LINE_IN_URI  # Home Assistant switches the karaoke machine in
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.poll()
        self.released_while_playing()
        self.assertTrue(any("another source" in m for m in logs.output), logs.output)
        self.mpd("play")
        self.assertFalse(self.taken_over())
        self.assertEqual(self.rf.current_uri, LINE_IN_URI)

    def test_zone_switched_to_another_stream_is_taken_away(self):
        self.cast_is_playing()
        self.rf.current_uri = "http://radio.invalid/live.mp3"
        self.poll()
        self.released_while_playing()

    def test_room_leaving_its_zone_is_taken_away(self):
        self.cast_is_playing()
        self.rf.zone_udn = None
        self.poll()
        self.released_while_playing()
        self.assertNotIn("connectRoomToZone", self.rf.api_calls())

    # -- taken away while paused

    def test_auto_power_off_while_paused_releases_without_pause(self):
        self.cast_is_playing()
        self.mpd("pause")  # the user paused on the phone
        self.poll()
        self.nothing_requested()
        self.rf.power = "AUTOMATIC_STANDBY"  # the Raumfeld's own auto power-off
        self.poll()
        self.assertEqual(self.requests(), [ra.RELEASE_CAST])
        self.rf.requests.clear()
        self.poll()
        self.assertEqual(self.rf.requests, [])  # no more polling once released

    def test_stopped_renderer_while_paused_is_expected(self):
        self.cast_is_playing()
        self.mpd("pause")
        self.rf.transport_state = "STOPPED"  # the stalled stream stops the renderer
        for _ in range(ra.STOPPED_GRACE_POLLS + 3):
            self.poll()
        self.nothing_requested()

    # -- resume and new sessions

    def test_user_resume_after_yield_wakes_the_room_and_plays_again(self):
        self.cast_is_playing()
        self.rf.power = "MANUAL_STANDBY"
        self.poll()
        self.released_while_playing()
        self.mpd("pause")  # our own pause 1 arrives as an MPD event
        self.mpd("play")  # an old receiver kept the session; the user presses play
        self.assertEqual(self.rf.api_calls()[-2:], ["leaveStandby", "getZones"])  # woken, seen awake
        self.assertEqual(self.rf.power, "ACTIVE")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()
        # ...and is watched again.
        self.rf.power = "MANUAL_STANDBY"
        self.poll()
        self.released_while_playing()

    def test_user_resume_after_a_line_in_yield_while_line_in_still_plays_takes_over(self):
        self.cast_is_playing()
        self.rf.current_uri = LINE_IN_URI
        self.poll()
        self.released_while_playing()
        self.mpd("pause", "play")  # an explicit resume wins
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()
        self.rf.current_uri = LINE_IN_URI  # ...and a later switch still ends the cast
        self.poll()
        self.released_while_playing()

    def test_new_cast_while_line_in_plays_takes_over_and_sends_nothing(self):
        # Home Assistant leaves the room on line-in, which reports PLAYING without audio.
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.mpd("play")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertTrue(any("line-in" in m for m in logs.output), logs.output)
        self.nothing_requested()
        self.poll()  # owned and watched like any cast
        self.nothing_requested()

    def test_new_cast_while_line_in_is_idle_takes_over(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "STOPPED"
        self.rf.current_uri = LINE_IN_URI
        self.mpd("play")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    def test_new_cast_session_after_release_takes_over_again(self):
        self.cast_is_playing()
        self.rf.power = "MANUAL_STANDBY"
        self.poll()
        self.released_while_playing()
        self.mpd("pause", "stop")  # our pause, then the receiver ends the session
        self.mpd("play")  # later: a new cast
        self.assertEqual(self.rf.power, "ACTIVE")
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    # -- normal operation

    def test_transient_stopped_within_grace_does_not_pause(self):
        self.cast_is_playing()
        self.rf.transport_state = "STOPPED"
        for _ in range(ra.STOPPED_GRACE_POLLS):
            self.poll()
        self.nothing_requested()
        self.rf.transport_state = "PLAYING"
        self.poll()
        self.rf.transport_state = "STOPPED"
        for _ in range(ra.STOPPED_GRACE_POLLS):
            self.poll()
        self.nothing_requested()  # the count restarted after the PLAYING poll
        self.poll()
        self.released_while_playing()
        self.assertNotIn("Play", self.rf.soap_actions())  # polling never restarts the renderer

    def test_track_changes_request_nothing_and_restart_the_renderer(self):
        self.cast_is_playing()
        for _ in range(3):
            self.rf.transport_state = "STOPPED"  # MPD httpd dropped the renderer between songs
            self.mpd("stop", "play")
            self.assertEqual(self.rf.soap_actions()[-1], "Play")
            self.poll()
        self.mpd("play")  # coalesced track change
        self.nothing_requested()

    def test_polling_while_stopped_ends_when_the_room_is_gone(self):
        self.cast_is_playing()
        self.mpd("stop")
        self.rf.transport_state = "STOPPED"
        for _ in range(ra.STOPPED_GRACE_POLLS + 3):
            self.poll()  # a stopped renderer is expected while MPD is stopped
        self.nothing_requested()
        self.rf.power = "AUTOMATIC_STANDBY"
        self.poll()
        self.rf.requests.clear()
        for _ in range(3):
            self.poll()
        self.assertEqual(self.rf.requests, [])
        self.assertIsNone(self.follower.seconds_until_deadline())
        self.nothing_requested()

    # -- after a normal session end (receiver stopped MPD)

    def test_session_end_then_auto_power_off_then_new_cast_wakes_and_plays(self):
        self.cast_is_playing()
        self.mpd("stop")  # the phone disconnected: the receiver stops and clears MPD
        self.poll()
        self.rf.power = "AUTOMATIC_STANDBY"  # minutes later
        self.rf.transport_state = "STOPPED"
        self.poll()
        self.nothing_requested()  # no pause, no release: there is no cast to end
        self.mpd("play")  # a new cast
        self.assertIn("leaveStandby", self.rf.api_calls())
        self.assertEqual(self.rf.power, "ACTIVE")
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    def test_song_change_gap_does_not_wake_and_playback_continues(self):
        self.cast_is_playing()
        self.rf.transport_state = "STOPPED"
        self.mpd("stop", "play")  # within one poll interval, room still ACTIVE
        self.assertNotIn("leaveStandby", self.rf.api_calls())
        self.assertEqual(self.rf.soap_actions()[-1], "Play")
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    def test_session_end_then_line_in_then_new_cast_takes_over(self):
        self.cast_is_playing()
        self.mpd("stop")
        self.rf.current_uri = LINE_IN_URI  # the user switches to the karaoke machine
        self.poll()
        self.nothing_requested()
        self.mpd("play")  # a new cast while line-in plays: the cast wins
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    def test_session_end_then_idle_line_in_then_new_cast_takes_over(self):
        self.cast_is_playing()
        self.mpd("stop")
        self.rf.current_uri = LINE_IN_URI
        self.rf.transport_state = "STOPPED"
        self.poll()
        self.mpd("play")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()

    def test_no_polling_while_yielded(self):
        self.cast_is_playing()
        self.rf.power = "MANUAL_STANDBY"
        self.poll()
        self.released_while_playing()
        self.mpd("pause")
        self.rf.requests.clear()
        self.poll()
        self.assertEqual(self.rf.requests, [])
        self.assertIsNone(self.follower.seconds_until_deadline())

    def test_polls_reuse_discovery_and_rediscover_after_an_error(self):
        self.cast_is_playing()
        searches = len(self.discover.searches)
        self.poll()
        self.poll()
        self.assertEqual(len(self.discover.searches), searches)
        self.rf.fail_paths.add("getZones")
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.poll()
        self.nothing_requested()  # a network hiccup is not "taken away"
        self.rf.fail_paths.clear()
        self.poll()
        # The host was searched for again at once and at the next poll; the zone renderer
        # never failed, so its cached address is kept.
        self.assertEqual(self.discover.searches[searches:], [SPEAKER_UDN, SPEAKER_UDN])
        self.nothing_requested()


class StuckRendererScenarioTest(unittest.TestCase):
    """Scenario: at a song change the renderer dropped the stream and stays TRANSITIONING;
    the adapter restarts it (Stop, Play) and does not end the cast."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.clock = FakeClock()
        self.follower = ra.Follower(make_controller(self.rf, FakeDiscovery(self.rf)),
                                    poll_seconds=POLL_S, clock=self.clock, stuck_seconds=15,
                                    start_gate_seconds=0)

    def poll(self):
        self.clock.now += POLL_S
        self.follower.tick()

    def test_stuck_renderer_is_restarted_and_plays_again(self):
        self.follower.on_mpd_state("play")
        self.rf.transport_state = "TRANSITIONING"
        self.rf.stuck = True
        self.rf.requests.clear()
        for _ in range(3):
            self.poll()
        self.assertNotIn("Stop", self.rf.soap_actions())
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            self.poll()
        self.assertEqual([a for a in self.rf.soap_actions() if a in ("Stop", "Play")],
                         ["Stop", "Play"])
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.poll()
        self.assertEqual(self.follower.take_mpd_requests(), [])
        self.assertFalse(self.follower.yielded)
        for method, path, action, body in self.rf.requests:
            self.assertNotIn("RenderingControl", path)
            self.assertNotIn("Volume", path + (action or "") + body)
            self.assertNotIn("Standby", path)


class StartGateScenarioTest(unittest.TestCase):
    """Scenario: a cast starts while the room sleeps or plays line-in. MPD is held at the
    start of the song until the zone renderer plays our stream, so nothing is missed."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.clock = FakeClock()
        self.follower = ra.Follower(make_controller(self.rf, FakeDiscovery(self.rf)),
                                    poll_seconds=POLL_S, clock=self.clock, start_gate_seconds=15)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)
        self.assertNotIn("Pause", self.rf.soap_actions())  # our gate pause stays in MPD

    def gate_tick(self):
        self.clock.now += ra.GATE_POLL_S
        self.follower.tick()

    def test_explicit_cast_over_line_in_is_held_until_the_renderer_plays(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("play", "1")
        self.assertEqual(self.rf.requests, [])  # MPD first, the room after
        self.assertEqual(self.follower.take_mpd_requests(), [ra.PAUSE_PLAYER])
        self.rf.stuck = True  # Play leads to TRANSITIONING: connecting and buffering
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("pause", "1")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.gate_tick()
        self.assertEqual(self.follower.take_mpd_requests(), [])
        with self.rf.lock:
            self.rf.transport_state = "PLAYING"
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.gate_tick()
        self.assertEqual(self.follower.take_mpd_requests(), [ra.RESUME_PLAYER])
        self.follower.on_mpd_state("play", "1")
        self.assertEqual(self.follower.take_mpd_requests(), [])
        self.assertFalse(self.follower.yielded)

    def test_waking_room_in_standby_is_held_and_released(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("play", "1")
        self.follower.take_mpd_requests()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("pause", "1")
        self.assertEqual(self.rf.api_calls()[-2:], ["leaveStandby", "getZones"])  # woken, seen awake
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.gate_tick()  # the fake renderer plays right after Play
        self.assertEqual(self.follower.take_mpd_requests(), [ra.RESUME_PLAYER])

    # -- the receiver's pre-roll: MPD starts on silence at the DIAL launch; the song
    # replaces it (play -> play, a new songid, no stop)

    def silence_held_over_line_in(self):
        self.rf.power = "ACTIVE"
        self.rf.transport_state = "PLAYING"
        self.rf.current_uri = LINE_IN_URI
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("play", "1")  # the silence
        self.assertEqual(self.follower.take_mpd_requests(), [ra.PAUSE_PLAYER])
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("pause", "1")

    def test_song_replacing_the_silence_during_the_gate_is_held_without_a_new_takeover(self):
        self.rf.stuck = True  # the renderer is still connecting and buffering
        self.silence_held_over_line_in()
        self.rf.requests.clear()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("play", "2")  # the song arrives
        self.assertEqual(self.follower.take_mpd_requests(), [ra.PAUSE_PLAYER])
        self.follower.on_mpd_state("pause", "2")
        self.assertEqual(self.rf.api_calls(), [])  # no wake, no look at the room
        self.assertEqual(self.rf.soap_actions(), [])  # no SetAVTransportURI, no Play
        self.gate_tick()
        self.assertEqual(self.follower.take_mpd_requests(), [])
        with self.rf.lock:
            self.rf.transport_state = "PLAYING"
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.gate_tick()
        self.assertEqual(self.follower.take_mpd_requests(), [ra.RESUME_PLAYER])
        self.follower.on_mpd_state("play", "2")
        self.assertEqual(self.follower.take_mpd_requests(), [])
        self.assertNotIn("SetAVTransportURI", self.rf.soap_actions())
        # A takeover from line-in remembered line-in as the source before: a song change
        # routed through a takeover would have overwritten it with our stream.
        self.assertEqual(self.follower.controller._source_before, LINE_IN_URI)

    def test_song_after_the_gate_opened_on_silence_plays_at_once(self):
        self.silence_held_over_line_in()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.gate_tick()  # the renderer plays the silence
        self.assertEqual(self.follower.take_mpd_requests(), [ra.RESUME_PLAYER])
        self.follower.on_mpd_state("play", "1")  # our pause 0
        self.rf.requests.clear()
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            self.follower.on_mpd_state("play", "2")  # the song arrives; the stream goes on
        self.assertTrue(any("speaker already playing" in m for m in logs.output), logs.output)
        self.assertEqual(self.follower.take_mpd_requests(), [])  # no pause: no gate
        self.assertIsNone(self.follower._gate)
        self.assertNotIn("leaveStandby", self.rf.api_calls())
        self.assertNotIn("SetAVTransportURI", self.rf.soap_actions())
        self.assertNotIn("Play", self.rf.soap_actions())
        self.assertEqual(self.rf.transport_state, "PLAYING")


class GateRetakeoverScenarioTest(unittest.TestCase):
    """Scenario (live incident): the user switches the speaker off and casts again a few
    seconds later. The speaker, still finishing its standby, accepts the takeover and
    drops it. The adapter takes over again instead of letting the cast fail."""

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.clock = FakeClock()
        self.follower = ra.Follower(make_controller(self.rf, FakeDiscovery(self.rf)),
                                    poll_seconds=POLL_S, clock=self.clock, start_gate_seconds=15)
        self.restarts = 0

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf, self.restarts)

    def requests(self):
        return self.follower.take_mpd_requests()

    def cast(self):
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("play", "1")
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER])
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.follower.on_mpd_state("pause", "1")  # run() reports our pause 1

    def gate_until_released(self):
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            for _ in range(GATE_LOOKS):
                self.clock.now += ra.GATE_POLL_S
                self.follower.tick()
                requests = self.requests()
                if requests:
                    break
        self.assertEqual(requests, [ra.RESUME_PLAYER])
        self.follower.on_mpd_state("play", "1")  # run() reports our pause 0
        return logs.output

    def poll(self):
        self.clock.now += POLL_S
        self.follower.tick()

    def plays(self):
        return [a for a in self.rf.soap_actions() if a == "Play"]

    def test_dropped_takeover_is_done_again_within_the_gate(self):
        self.rf.drop_plays = 1
        self.cast()
        self.assertEqual(self.rf.transport_state, "STOPPED")  # accepted, dropped
        logs = self.gate_until_released()
        self.assertTrue(any("taking over again" in m for m in logs), logs)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.assertEqual(len(self.plays()), 2)
        for _ in range(4):
            self.poll()
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.follower.yielded)

    def test_room_back_in_standby_during_the_gate_is_woken_again(self):
        self.rf.drop_plays = 1
        self.cast()
        with self.rf.lock:
            self.rf.power = "MANUAL_STANDBY"  # its shutdown completes after our wake
        self.gate_until_released()
        self.assertEqual(self.rf.api_calls().count("leaveStandby"), 2)
        self.assertEqual(self.rf.power, "ACTIVE")
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_slow_wake_is_waited_for_and_needs_no_second_takeover(self):
        self.rf.wake_after_polls = 4  # still shutting down when woken
        self.cast()
        self.assertEqual(self.rf.power, "ACTIVE")
        logs = self.gate_until_released()
        self.assertFalse(any("again" in m for m in logs), logs)
        self.assertEqual(len(self.plays()), 1)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_renderer_never_started_after_the_gate_gets_one_more_takeover(self):
        self.rf.drop_plays = 1 + ra.GATE_MAX_RETAKES
        self.cast()
        logs = self.gate_until_released()
        timeout = [m for m in logs if "anyway" in m]
        self.assertTrue(timeout and timeout[0].startswith("WARNING:"), logs)
        self.assertIn("renderer 'STOPPED'", timeout[0])
        self.assertIn("room 'not_playing'", timeout[0])
        for _ in range(ra.STOPPED_GRACE_POLLS):
            self.poll()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.rf.transport_state, "PLAYING")
        for _ in range(4):
            self.poll()
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.follower.yielded)

    def gate_until_requests(self):
        with self.assertLogs("raumfeld_adapter", "INFO") as logs:
            for _ in range(GATE_LOOKS):
                self.clock.now += ra.GATE_POLL_S
                self.follower.tick()
                requests = self.requests()
                if requests:
                    return requests, logs.output
        self.fail("the gate never asked MPD for anything")

    def assert_respected(self, uri):
        """The gate yields to the source: MPD stays paused, the cast is released."""
        requests, logs = self.gate_until_requests()
        self.assertEqual(requests, [ra.RELEASE_CAST])  # no pause 0
        self.assertFalse(any("again" in m for m in logs), logs)
        self.assertEqual(self.rf.soap_actions().count("SetAVTransportURI"), 1)
        self.assertEqual(self.rf.current_uri, uri)
        self.assertTrue(self.follower.yielded)
        for _ in range(4):
            self.poll()
        self.assertEqual(self.requests(), [])
        self.assertEqual(self.rf.current_uri, uri)

    def test_line_in_started_during_the_gate_after_standby_is_respected(self):
        self.rf.drop_plays = 1
        self.cast()  # from standby: nothing played before
        with self.rf.lock:  # someone switches the room to line-in meanwhile
            self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.assert_respected(LINE_IN_URI)

    def test_dropped_takeover_of_line_in_is_taken_over_again(self):
        # A karaoke machine on line-in; the cast takes over, the speaker drops it.
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.rf.drop_takeovers = 1
        self.cast()
        self.assertEqual(self.rf.current_uri, LINE_IN_URI)
        logs = self.gate_until_released()
        self.assertTrue(any("taking over again" in m for m in logs), logs)
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        for _ in range(4):
            self.poll()
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.follower.yielded)

    def test_radio_started_after_a_dropped_takeover_of_line_in_is_respected(self):
        self.rf.power = "ACTIVE"
        self.rf.current_uri, self.rf.transport_state = LINE_IN_URI, "PLAYING"
        self.rf.drop_takeovers = 1
        self.cast()
        with self.rf.lock:  # someone starts a radio station instead
            self.rf.current_uri = RADIO_URI
        self.assert_respected(RADIO_URI)

    def test_renderer_hanging_in_transitioning_is_restarted_within_the_gate(self):
        self.rf.stuck = True  # Play leaves it TRANSITIONING; Stop, Play brings it back
        self.cast()
        logs = self.gate_until_released()
        self.restarts = 1
        self.assertEqual([a for a in self.rf.soap_actions() if a in ("Stop", "Play")],
                         ["Play", "Stop", "Play"])
        self.assertEqual(self.rf.soap_actions().count("SetAVTransportURI"), 1)
        self.assertFalse(any("taking over again" in m for m in logs), logs)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_renderer_hanging_after_a_slow_wake_is_still_restarted_within_the_gate(self):
        # Woken from standby, the room takes 8 s to report ACTIVE: longer than GATE_RETAKE_S.
        def sleep(seconds):
            self.clock.now += seconds
        controller = make_controller(self.rf, FakeDiscovery(self.rf))
        controller.sleep, controller.clock = sleep, self.clock
        self.follower = ra.Follower(controller, poll_seconds=POLL_S, clock=self.clock,
                                    start_gate_seconds=15)
        self.rf.wake_after_polls = int(8 / ra.WAKE_POLL_S)
        self.rf.stuck = True
        self.cast()
        self.assertEqual(self.clock.now, 1008)
        logs = self.gate_until_released()
        self.restarts = 1
        self.assertEqual([a for a in self.rf.soap_actions() if a in ("Stop", "Play")],
                         ["Play", "Stop", "Play"])
        self.assertTrue(any("speaker plays" in m for m in logs), logs)
        self.assertEqual(self.rf.transport_state, "PLAYING")

    def test_user_stopping_the_speaker_after_it_played_is_still_respected(self):
        self.cast()
        self.gate_until_released()
        self.poll()
        with self.rf.lock:
            self.rf.transport_state = "STOPPED"  # e.g. stopped from the Raumfeld app
        for _ in range(ra.STOPPED_GRACE_POLLS):
            self.poll()
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.poll()
        self.assertEqual(self.requests(), [ra.PAUSE_PLAYER, ra.RELEASE_CAST])
        self.assertEqual(len(self.plays()), 1)


class SpeakerYieldRunTest(unittest.TestCase):
    """End to end through run(): the adapter talks to MPD over its own connection."""

    def run_until(self, mpd, rf, follower, mutate, done):
        def should_stop():
            with rf.lock:
                mutate()
            return done()

        run_bounded(self, follower, mpd, should_stop, sleep=lambda s: time.sleep(0.05))

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.follower = ra.Follower(make_controller(self.rf, FakeDiscovery(self.rf)),
                                    poll_seconds=0.05, start_gate_seconds=0)

    def tearDown(self):
        assert_speaker_left_alone(self, self.rf)

    def standby_once_playing(self):
        if self.rf.transport_state == "PLAYING" and self.rf.power == "ACTIVE":
            self.rf.power = "MANUAL_STANDBY"

    def test_standby_while_playing_sends_pause_1_then_release_once_and_never_stop(self):
        mpd = FakeMpd([["play"]])
        self.addCleanup(mpd.close)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.run_until(mpd, self.rf, self.follower, self.standby_once_playing,
                           lambda: self.follower.mpd_state == "pause")
        self.assertEqual(self.follower.mpd_state, "pause")
        cmds = [c for c in mpd.commands if c not in ("status", "idle player", "noidle")]
        self.assertEqual(cmds, ["pause 1", "sendmessage ytcast release"])
        self.assertEqual(self.rf.power, "MANUAL_STANDBY")

    def test_release_without_subscriber_is_logged_and_not_fatal(self):
        mpd = FakeMpd([["play"]])
        mpd.message_subscribers = False  # an old receiver: MPD answers ACK
        self.addCleanup(mpd.close)
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            self.run_until(mpd, self.rf, self.follower, self.standby_once_playing,
                           lambda: self.follower.mpd_state == "pause")
        self.assertEqual(self.follower.mpd_state, "pause")
        self.assertEqual(mpd.connections, 1)  # no reconnect
        self.assertEqual(mpd.commands.count("sendmessage ytcast release"), 1)
        self.assertTrue(any("ytcast" in m for m in logs.output), logs.output)

    def test_receiver_ending_the_session_right_away_ends_the_yield(self):
        mpd = FakeMpd([["play"]])
        mpd.stop_on_message = True  # our pause and its stop reach us as one MPD event
        self.addCleanup(mpd.close)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.run_until(mpd, self.rf, self.follower, self.standby_once_playing,
                           lambda: self.follower.mpd_state == "stop")
        self.assertEqual(self.follower.mpd_state, "stop")
        self.assertFalse(self.follower.yielded)  # a new cast session may take over again

    def test_release_survives_a_connection_lost_on_pause(self):
        mpd = FakeMpd([["play"], ["play"]])
        mpd.drop_on_first_pause = True
        self.addCleanup(mpd.close)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            self.run_until(mpd, self.rf, self.follower, self.standby_once_playing,
                           lambda: "sendmessage ytcast release" in mpd.commands
                           and self.follower.mpd_state == "pause")
        self.assertEqual(mpd.connections, 2)
        self.assertEqual(mpd.commands.count("sendmessage ytcast release"), 1)
        cmds = [c for c in mpd.commands if c not in ("status", "idle player", "noidle")]
        self.assertEqual(cmds, ["pause 1", "pause 1", "sendmessage ytcast release"])

    def test_track_changes_send_nothing_to_mpd(self):
        mpd = FakeMpd([["play", "stop", "play", "play"]])
        self.addCleanup(mpd.close)
        self.run_until(mpd, self.rf, self.follower, lambda: None,
                       lambda: mpd.commands.count("status") >= 4)
        cmds = [c for c in mpd.commands if c not in ("status", "idle player", "noidle")]
        self.assertEqual(cmds, [])


if __name__ == "__main__":
    unittest.main()
