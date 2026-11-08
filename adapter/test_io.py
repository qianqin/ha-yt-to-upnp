"""I/O edges: MPD client and main loop against a fake MPD server, SSDP against a local responder."""

import os
import socket
import threading
import time
import unittest
from unittest import mock

import raumfeld_adapter as ra
from fakes import SPEAKER_UDN, FakeMpd, run_bounded
from test_logic import FakeClock, RecordingController


class MpdClientTest(unittest.TestCase):
    def test_status_and_idle(self):
        mpd = FakeMpd([["stop", "play"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        self.assertEqual(client.status().state, "stop")
        self.assertTrue(client.wait_player(None))
        self.assertEqual(client.status().state, "play")
        self.assertEqual(mpd.commands, ["status", "idle player", "status"])

    def test_idle_timeout_sends_noidle_and_connection_stays_usable(self):
        mpd = FakeMpd([["pause"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        started = time.monotonic()
        self.assertFalse(client.wait_player(0.2))
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(client.status().state, "pause")
        self.assertEqual(mpd.commands, ["idle player", "noidle", "status"])

    def test_status_reports_the_song_id(self):
        mpd = FakeMpd([[("play", "7"), "stop"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        self.assertEqual(client.status(), ("play", "7"))
        client.wait_player(None)
        self.assertEqual(client.status(), ("stop", None))  # no current song

    def test_resume_sends_pause_0(self):
        mpd = FakeMpd([[("play", "7")]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        client.pause()
        self.assertEqual(client.status(), ("pause", "7"))
        client.resume()
        self.assertEqual(client.status(), ("play", "7"))
        self.assertEqual(mpd.commands, ["pause 1", "status", "pause 0", "status"])

    def test_non_positive_idle_timeout_is_rejected(self):
        mpd = FakeMpd([["stop"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.wait_player(0.0)
        self.assertEqual(client.status().state, "stop")

    def test_pause_and_release_message(self):
        mpd = FakeMpd([["play"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        client.pause()
        client.send_message(ra.CAST_CHANNEL, ra.CAST_RELEASE_MESSAGE)
        self.assertEqual(client.status().state, "pause")
        self.assertEqual(mpd.commands, ["pause 1", "sendmessage ytcast release", "status"])

    def test_send_message_without_subscriber_raises_mpd_error(self):
        mpd = FakeMpd([["play"]])
        mpd.message_subscribers = False
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        with self.assertRaises(ra.MpdError):
            client.send_message(ra.CAST_CHANNEL, ra.CAST_RELEASE_MESSAGE)
        self.assertEqual(client.status().state, "play")  # the connection stays usable

    def test_send_message_refuses_unsafe_tokens(self):
        mpd = FakeMpd([["play"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        for channel, text in (("yt cast", "release"), ("ytcast", "release\nstop"), ("", "x")):
            with self.subTest(channel=channel, text=text), self.assertRaises(ValueError):
                client.send_message(channel, text)
        self.assertEqual(mpd.commands, [])

    def test_server_gone_raises_connection_error(self):
        mpd = FakeMpd([["play"]], close_after=True)
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        with self.assertRaises(ConnectionError):
            client.wait_player(None)

    def test_not_mpd_is_rejected(self):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        self.addCleanup(srv.close)

        def answer():
            conn, _ = srv.accept()
            conn.sendall(b"HTTP/1.1 400 Bad Request\r\n")
            conn.close()

        threading.Thread(target=answer, daemon=True).start()
        with self.assertRaises(ra.MpdError):
            ra.MpdClient("127.0.0.1", srv.getsockname()[1])


def raw_server(test, handler):
    """One-shot TCP server on 127.0.0.1 running handler(conn) for each connection."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(5)
    test.addCleanup(srv.close)

    def serve():
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            threading.Thread(target=_handle, args=(conn,), daemon=True).start()

    def _handle(conn):
        try:
            handler(conn)
        except OSError:
            pass
        finally:
            conn.close()

    threading.Thread(target=serve, daemon=True).start()
    return srv.getsockname()[1]


class MpdLimitsTest(unittest.TestCase):
    def test_overlong_line_is_rejected(self):
        port = raw_server(self, lambda conn: conn.sendall(b"OK MPD " + b"x" * 70000))
        with self.assertRaises(ra.MpdError):
            ra.MpdClient("127.0.0.1", port)


class HttpLimitsTest(unittest.TestCase):
    HEAD = b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\r\n"

    def test_small_body_is_returned(self):
        port = raw_server(self, lambda conn: (conn.recv(4096), conn.sendall(self.HEAD + b"<a/>")))
        self.assertEqual(ra.http_get("http://127.0.0.1:%d/x" % port), b"<a/>")

    def test_endless_body_is_cut_at_max_body(self):
        def endless(conn):
            conn.recv(4096)
            conn.sendall(self.HEAD)
            for _ in range(64):  # 4 MiB: far past MAX_BODY, but bounded if the cap regresses
                conn.sendall(b"a" * 65536)

        port = raw_server(self, endless)
        with self.assertRaises(ra.RaumfeldError):
            ra.http_get("http://127.0.0.1:%d/x" % port)

    def test_slow_body_hits_total_deadline(self):
        def trickle(conn):
            conn.recv(4096)
            conn.sendall(self.HEAD)
            for _ in range(120):  # ~6 s, so a broken deadline fails the timing check, not CI
                conn.sendall(b"a")
                time.sleep(0.05)

        port = raw_server(self, trickle)
        started = time.monotonic()
        with self.assertRaises(ra.RaumfeldError):
            ra.http_get("http://127.0.0.1:%d/x" % port, deadline_s=0.5)
        self.assertLess(time.monotonic() - started, 2)

    def test_slow_headers_hit_total_deadline(self):
        def slow_headers(conn):
            conn.recv(4096)
            conn.sendall(b"HTTP/1.1 200 OK\r\n")
            for i in range(20):  # bounded, so a regression fails instead of hanging
                time.sleep(0.3)
                conn.sendall(b"X-Slow-%d: 1\r\n" % i)

        port = raw_server(self, slow_headers)
        started = time.monotonic()
        with self.assertRaises(ra.RaumfeldError):
            ra.http_get("http://127.0.0.1:%d/x" % port, deadline_s=0.5)
        self.assertLess(time.monotonic() - started, 1.5)

    def test_redirect_loop_stops_after_max_redirects(self):
        requests = []

        def loop(conn):
            requests.append(conn.recv(4096))
            conn.sendall(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /again\r\n"
                         b"Content-Length: 0\r\nConnection: close\r\n\r\n")

        port = raw_server(self, loop)
        started = time.monotonic()
        with self.assertRaisesRegex(ra.RaumfeldError, "too many redirects"):
            ra.http_get("http://127.0.0.1:%d/x" % port)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(len(requests), 4)  # the original request plus 3 redirects, pinned

    def test_post_redirect_is_not_followed(self):
        requests = []

        def redirect(conn):
            requests.append(conn.recv(65536))
            conn.sendall(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /elsewhere\r\n"
                         b"Content-Length: 0\r\nConnection: close\r\n\r\n")

        port = raw_server(self, redirect)
        with self.assertRaisesRegex(ra.RaumfeldError, "HTTP 307"):
            ra.soap_call("http://127.0.0.1:%d/ctrl" % port, "Stop", [])
        self.assertEqual(len(requests), 1)
        self.assertTrue(requests[0].startswith(b"POST /ctrl "))

    def test_non_http_scheme_is_refused(self):
        with self.assertRaises(ra.RaumfeldError):
            ra.http_get("file://" + os.path.abspath(__file__))


class RecordingFollower:
    def __init__(self, stop_after):
        self.states = []
        self.ticks = 0
        self.stop_after = stop_after

    def on_mpd_state(self, state, songid=None):
        self.states.append(state)

    def tick(self):
        self.ticks += 1

    def take_mpd_requests(self):
        return []

    def seconds_until_deadline(self):
        return None

    def done(self):
        return len(self.states) >= self.stop_after


class RunLoopTest(unittest.TestCase):
    def test_follows_states_and_reconnects_with_backoff(self):
        # Connection 1 dies after two states, connection 2 is refused-free and continues.
        mpd = FakeMpd([["stop", "play"], ["pause"]], close_after=True)
        self.addCleanup(mpd.close)
        follower = RecordingFollower(stop_after=3)
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=sleeps.append, should_stop=follower.done)
        self.assertEqual(follower.states, ["stop", "play", "pause"])
        self.assertEqual(follower.ticks, 3)  # after each connect and each wake-up
        self.assertEqual(sleeps, [1])

    def test_backoff_resets_after_successful_reconnect(self):
        mpd = FakeMpd([["stop"], ["stop"], ["stop"]], close_after=True)
        self.addCleanup(mpd.close)
        follower = RecordingFollower(stop_after=3)
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=sleeps.append, should_stop=follower.done)
        self.assertEqual(sleeps, [1, 1])

    def test_failed_takeover_is_retried_after_reconnect_without_reconnect_loop(self):
        clock = FakeClock()
        ctl = RecordingController()
        ctl.fail = OSError("speaker unreachable")
        follower = ra.Follower(ctl, poll_seconds=5, clock=clock, start_gate_seconds=0)
        # Connection 1 reports play then drops; connection 2 stays open in idle.
        mpd = FakeMpd([["play"], ["play"]], close_after=False)
        mpd.close_first = True
        self.addCleanup(mpd.close)
        sleeps = []

        def sleep(seconds):
            sleeps.append(seconds)
            ctl.fail = None
            clock.now += 400  # the outage outlasts the retry delay

        with self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=sleep,
                        should_stop=lambda: len(ctl.calls) >= 2 or len(sleeps) > 3)
        self.assertEqual(ctl.calls, ["ensure_playing", "ensure_playing"])
        self.assertEqual(sleeps, [1])

    def test_past_deadline_wait_is_clamped_positive(self):
        clock = FakeClock()
        ctl = RecordingController()
        ctl.fail = OSError("speaker unreachable")
        follower = ra.Follower(ctl, poll_seconds=5, clock=clock, start_gate_seconds=0)
        mpd = FakeMpd([["play"]])
        self.addCleanup(mpd.close)
        ticks = []
        real_tick = follower.tick

        def tick():
            ticks.append(1)
            real_tick()  # the retry fails and re-arms ...
            clock.now += 1000  # ... but the next deadline is already passed again

        follower.tick = tick
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=sleeps.append,
                        should_stop=lambda: len(ticks) >= 3 or bool(sleeps))
        self.assertEqual(sleeps, [])
        self.assertEqual(mpd.commands.count("noidle"), 2)

    def test_start_gate_pauses_before_the_takeover_and_resumes_once_playing(self):
        ctl = RecordingController()
        follower = ra.Follower(ctl, poll_seconds=5, start_gate_seconds=5)
        mpd = FakeMpd([[("play", "1")]])
        self.addCleanup(mpd.close)
        real_ensure = ctl.ensure_playing

        def ensure_playing(wake=True):
            ctl.calls.append("mpd sent: %r" % [c for c in mpd.commands if c.startswith("pause")])
            return real_ensure(wake)

        ctl.ensure_playing = ensure_playing
        with self.assertLogs("raumfeld_adapter", "INFO"):
            run_bounded(self, follower, mpd, sleep=lambda s: None,
                        should_stop=lambda: "pause 0" in mpd.commands)
        self.assertEqual(ctl.calls[:3], ["mpd sent: ['pause 1']", "ensure_playing", "plays_stream"])
        self.assertEqual([c for c in mpd.commands if c.startswith("pause")], ["pause 1", "pause 0"])

    def test_failed_takeover_in_the_gate_resumes_mpd_right_away(self):
        ctl = RecordingController()
        ctl.fail = OSError("speaker unreachable")
        follower = ra.Follower(ctl, poll_seconds=5, start_gate_seconds=5)
        mpd = FakeMpd([[("play", "1")]])
        self.addCleanup(mpd.close)
        takeovers = []
        real_ensure = ctl.ensure_playing

        def ensure_playing(wake=True):
            takeovers.append((time.monotonic(), list(mpd.commands)))
            return real_ensure(wake)

        ctl.ensure_playing = ensure_playing
        started = time.monotonic()
        # A short retry delay, so the run goes past the idle event of our own pause 0 and on
        # to the retry within the test's time limit.
        with mock.patch.object(ra, "PLAY_RETRY_S", 1.5), \
                self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=lambda s: None,
                        should_stop=lambda: len(takeovers) >= 2)
        # pause 0 went out right after the failure, not after the next MPD event or the retry.
        resumed_by = takeovers[1][1]
        self.assertEqual([c for c in resumed_by if c.startswith("pause")], ["pause 1", "pause 0"])
        self.assertLess(takeovers[0][0] - started, 2)
        # The echo of our pause 0 started no new gate; the retry waited for its delay and
        # did not hold MPD again.
        self.assertGreaterEqual(takeovers[1][0] - takeovers[0][0], 1.4)
        self.assertEqual([c for c in mpd.commands if c.startswith("pause")],
                         ["pause 1", "pause 0"])
        self.assertEqual(ctl.calls, ["ensure_playing", "ensure_playing"])

    def test_mpd_dropping_a_client_silent_during_slow_speaker_io_is_logged_truthfully(self):
        # The incident: a speaker request ran into its deadline while the adapter was out
        # of `idle`; MPD's connection_timeout hung up, and the next command found it closed.
        mpd = FakeMpd([["play"], ["play"]])
        mpd.connection_timeout = 0.2
        self.addCleanup(mpd.close)
        follower = RecordingFollower(stop_after=2)
        follower.seconds_until_deadline = lambda: 0.05
        real_tick = follower.tick

        def slow_tick():
            real_tick()
            if follower.ticks == 2:  # the first tick after an idle timeout
                time.sleep(0.5)  # blocked on the speaker

        follower.tick = slow_tick
        with self.assertLogs("raumfeld_adapter", "WARNING") as logs:
            run_bounded(self, follower, mpd, sleep=lambda s: None, should_stop=follower.done)
        self.assertEqual(mpd.connections, 2)
        message = [r.getMessage() for r in logs.records if "MPD" in r.getMessage()][0]
        self.assertRegex(message, r"MPD closed the connection after 0\.[5-9] s without a command")
        self.assertIn("connection_timeout", message)

    def test_unreachable_mpd_backs_off_exponentially(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()  # nothing listens here now
        sleeps = []

        def should_stop():
            return len(sleeps) >= 8

        with self.assertLogs("raumfeld_adapter", "WARNING"):
            ra.run(RecordingFollower(99), lambda: ra.MpdClient("127.0.0.1", port),
                   sleep=sleeps.append, should_stop=should_stop)
        self.assertEqual(sleeps, [1, 2, 4, 8, 16, 30, 30, 30])


class SsdpTest(unittest.TestCase):
    def responder(self, replies, drop_first=0):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind(("127.0.0.1", 0))
        sock.settimeout(5)
        self.addCleanup(sock.close)
        self.received = []

        def serve():
            try:
                while True:
                    data, addr = sock.recvfrom(4096)
                    self.received.append(data.decode())
                    if len(self.received) <= drop_first:
                        continue
                    for r in replies:
                        sock.sendto(r.encode(), addr)
            except OSError:
                pass

        threading.Thread(target=serve, daemon=True).start()
        return sock.getsockname()

    def reply(self, st, location):
        return ("HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nST: %s\r\n"
                "USN: %s\r\nLOCATION: %s\r\n\r\n" % (st, st, location))

    def test_finds_device_by_udn_and_ignores_other_replies(self):
        addr = self.responder([
            self.reply("uuid:00000000-0000-0000-0000-0000000000ff", "http://127.0.0.1:1/wrong.xml"),
            self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml"),
        ])
        host, location = ra.ssdp_discover(SPEAKER_UDN, timeout=3, addr=addr)
        self.assertEqual((host, location), ("127.0.0.1", "http://127.0.0.1:2/desc.xml"))
        msg = self.received[0]
        self.assertTrue(msg.startswith("M-SEARCH * HTTP/1.1\r\n"))
        self.assertIn("ST: %s\r\n" % SPEAKER_UDN, msg)
        self.assertIn('MAN: "ssdp:discover"\r\n', msg)
        self.assertIn("MX: 2\r\n", msg)

    def test_resends_each_second(self):
        addr = self.responder([self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml")], drop_first=1)
        ra.ssdp_discover(SPEAKER_UDN, timeout=4, addr=addr)
        self.assertEqual(len(self.received), 2)

    def test_accepts_bare_newline_replies(self):
        reply = self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml").replace("\r\n", "\n")
        addr = self.responder([reply])
        self.assertEqual(ra.ssdp_discover(SPEAKER_UDN, timeout=3, addr=addr),
                         ("127.0.0.1", "http://127.0.0.1:2/desc.xml"))

    def test_not_found_times_out(self):
        addr = self.responder([])
        started = time.monotonic()
        with self.assertRaises(ra.DeviceNotFound):
            ra.ssdp_discover(SPEAKER_UDN, timeout=1.5, addr=addr)
        self.assertLess(time.monotonic() - started, 3)
        self.assertEqual(len(self.received), 2)


if __name__ == "__main__":
    unittest.main()
