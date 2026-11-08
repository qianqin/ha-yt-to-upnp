"""Feature: the speaker's own controls win -- turning the Raumfeld room off (by button or
by its own auto power-off) or switching it to another source ends the cast, and the
adapter stays away until the user resumes from the phone.

Real Follower + Controller against the fake Raumfeld host; MPD events are fed by hand.
"""

import time
import unittest

import raumfeld_adapter as ra
from fakes import (LINE_IN_URI, ROOM_UDN, SPEAKER_UDN, STREAM_URL, ZONE_UDN, FakeDiscovery,
                   FakeMpd, FakeRaumfeld)
from test_logic import FakeClock

POLL_S = 5


def make_controller(rf, discover):
    config = ra.Config.from_env({
        "RAUMFELD_ROOM_UDN": ROOM_UDN,
        "RAUMFELD_SPEAKER_UDN": SPEAKER_UDN,
        "STREAM_URL": STREAM_URL,
    })
    return ra.Controller(config, discover, api_port=rf.port, sleep=lambda s: None)


def assert_speaker_left_alone(test, rf):
    """In every scenario: no volume, and the adapter never stops or powers down the room
    itself (the Raumfeld's own auto power-off does that)."""
    for method, path, action, body in rf.requests:
        test.assertNotIn("RenderingControl", path)
        test.assertNotIn("Volume", path + (action or "") + body)
        test.assertNotIn("Standby", path.replace("leaveStandby", ""))
        test.assertNotEqual(action, "Stop")


class SpeakerYieldTest(unittest.TestCase):
    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.discover = FakeDiscovery(self.rf)
        self.clock = FakeClock()
        self.follower = ra.Follower(make_controller(self.rf, self.discover),
                                    poll_seconds=POLL_S, clock=self.clock)

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
        self.assertEqual(self.rf.api_calls()[-1], "leaveStandby")
        self.assertEqual(self.rf.power, "ACTIVE")
        self.assertEqual(self.rf.current_uri, STREAM_URL)
        self.assertEqual(self.rf.transport_state, "PLAYING")
        self.nothing_requested()
        # ...and is watched again.
        self.rf.power = "MANUAL_STANDBY"
        self.poll()
        self.released_while_playing()

    def test_user_resume_while_line_in_still_plays_does_not_take_over(self):
        self.cast_is_playing()
        self.rf.current_uri = LINE_IN_URI
        self.poll()
        self.released_while_playing()
        self.mpd("pause", "play")
        self.assertFalse(self.taken_over())
        self.assertEqual(self.rf.current_uri, LINE_IN_URI)
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

    def test_polling_stops_when_mpd_stops(self):
        self.cast_is_playing()
        self.poll()
        self.assertEqual(self.rf.soap_actions(), ["GetTransportInfo", "GetMediaInfo"])
        self.mpd("stop")
        self.rf.requests.clear()
        for _ in range(3):
            self.poll()
        self.assertEqual(self.rf.requests, [])
        self.assertIsNone(self.follower.seconds_until_deadline())

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
        self.assertEqual(self.discover.searches[searches:], [SPEAKER_UDN, ZONE_UDN])
        self.nothing_requested()


class SpeakerYieldRunTest(unittest.TestCase):
    """End to end through run(): the adapter talks to MPD over its own connection."""

    def run_until(self, mpd, rf, follower, mutate, done):
        deadline = time.monotonic() + 5

        def should_stop():
            with rf.lock:
                mutate()
            return done() or time.monotonic() > deadline

        ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port), sleep=lambda s: None,
               should_stop=should_stop)

    def setUp(self):
        self.rf = FakeRaumfeld()
        self.addCleanup(self.rf.close)
        self.follower = ra.Follower(make_controller(self.rf, FakeDiscovery(self.rf)),
                                    poll_seconds=0.05)

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

    def test_track_changes_send_nothing_to_mpd(self):
        mpd = FakeMpd([["play", "stop", "play", "play"]])
        self.addCleanup(mpd.close)
        self.run_until(mpd, self.rf, self.follower, lambda: None,
                       lambda: mpd.commands.count("status") >= 4)
        cmds = [c for c in mpd.commands if c not in ("status", "idle player", "noidle")]
        self.assertEqual(cmds, [])


if __name__ == "__main__":
    unittest.main()
