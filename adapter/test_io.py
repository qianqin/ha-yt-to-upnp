"""I/O edges: MPD client and main loop against a fake MPD server, SSDP against a local responder."""

import errno
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
        self.assertEqual(client.status(), ("play", "7", 0.0))
        client.wait_player(None)
        self.assertEqual(client.status(), ("stop", None, None))  # no current song

    def test_status_reports_how_far_into_the_song(self):
        mpd = FakeMpd([[("pause", "7", 12.5)]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        self.assertEqual(client.status(), ("pause", "7", 12.5))

    def test_resume_sends_pause_0(self):
        mpd = FakeMpd([[("play", "7")]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        client.pause()
        self.assertEqual(client.status()[:2], ("pause", "7"))
        client.resume()
        self.assertEqual(client.status()[:2], ("play", "7"))
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


class BrokenSocket:
    """Stands in for the MPD socket after MPD reset the connection: recv and sendall raise
    what the OS reports then (deterministic, unlike a real RST racing a FIN)."""

    def __init__(self, recv_error=None, send_error=None, recv_after_s=0.0,
                 setsockopt_error=None):
        self.recv_error = recv_error
        self.send_error = send_error
        self.recv_after_s = recv_after_s
        self.setsockopt_error = setsockopt_error
        self.sent = []
        self.closed = False

    def settimeout(self, timeout):
        pass

    def setsockopt(self, *args):
        if self.setsockopt_error:
            raise self.setsockopt_error

    def sendall(self, data):
        if self.send_error:
            raise self.send_error
        self.sent.append(data)

    def recv(self, size):
        time.sleep(self.recv_after_s)
        if self.recv_error:
            raise self.recv_error
        return b""

    def close(self):
        self.closed = True


class ScriptedSocket(BrokenSocket):
    """recv returns (or raises) the scripted replies in order, then EOF; the first after
    recv_after_s seconds."""

    def __init__(self, replies, recv_after_s=0.0):
        super().__init__(recv_after_s=recv_after_s)
        self.replies = list(replies)

    def recv(self, size):
        time.sleep(self.recv_after_s)
        self.recv_after_s = 0.0
        reply = self.replies.pop(0) if self.replies else b""
        if isinstance(reply, BaseException):
            raise reply
        return reply


class MpdConnectionLostTest(unittest.TestCase):
    """MPD hangs up on a client silent outside `idle` for its connection_timeout. A lost
    connection names that only when the adapter had been silent outside idle for at least
    TIMEOUT_HINT_S; a drop during idle (an MPD restart) is reported plainly, however it
    shows: an empty read, a reset on recv, or a broken pipe or reset on send."""

    TIMEOUT_HINT = "connection_timeout"

    def client(self, test_silence_s=0.0, sock=None):
        """A connected client; with sock, that stub replaces (and closes) the real socket."""
        mpd = FakeMpd([["play"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        client._quiet_since -= test_silence_s  # as if the adapter had waited on the speaker
        if sock is not None:
            client.sock.close()
            client.sock = sock
        return client

    def test_empty_read_after_silence_outside_idle_names_the_timeout(self):
        def hang_up_after_a_command(conn):
            conn.sendall(b"OK MPD 0.23.5\n")
            conn.recv(4096)
            conn.shutdown(socket.SHUT_WR)  # an orderly close: the client reads EOF
            time.sleep(1)

        client = ra.MpdClient("127.0.0.1", raw_server(self, hang_up_after_a_command))
        self.addCleanup(client.close)
        client._quiet_since -= 70
        with self.assertRaises(ConnectionError) as cm:
            client.status()
        self.assertRegex(str(cm.exception), r"after 70\.\d s without a command")
        self.assertIn(self.TIMEOUT_HINT, str(cm.exception))

    def test_reset_after_silence_outside_idle_names_the_timeout(self):
        for name, sock in (("recv", BrokenSocket(recv_error=ConnectionResetError(54, "reset"))),
                           ("send pipe", BrokenSocket(send_error=BrokenPipeError(32, "pipe"))),
                           ("send reset",
                            BrokenSocket(send_error=ConnectionResetError(54, "reset")))):
            with self.subTest(name):
                client = self.client(test_silence_s=70, sock=sock)
                with self.assertRaises(ConnectionError) as cm:
                    client.status()
                self.assertRegex(str(cm.exception), r"after 70\.\d s without a command")
                self.assertIn(self.TIMEOUT_HINT, str(cm.exception))
                self.assertNotIn("Errno", str(cm.exception))

    def test_reset_while_connecting_is_reported_plainly(self):
        # A reset that lands before the banner shows at whichever step comes first (on
        # macOS the connect itself, or EINVAL from setsockopt); stubbed, so no step races.
        reset = ConnectionResetError(54, "Connection reset by peer")
        for step, connect in (
                ("connect", mock.Mock(side_effect=reset)),
                ("keepalive", mock.Mock(return_value=BrokenSocket(
                    setsockopt_error=OSError(errno.EINVAL, "Invalid argument")))),
                ("banner", mock.Mock(return_value=BrokenSocket(recv_error=reset)))):
            with self.subTest(step), mock.patch.object(ra.socket, "create_connection", connect):
                with self.assertRaises(ConnectionError) as cm:
                    ra.MpdClient("127.0.0.1", 9)
                self.assertEqual(str(cm.exception), "MPD closed the connection")
                if step != "connect":
                    self.assertTrue(connect.return_value.closed)  # no socket left open

    def test_short_silence_outside_idle_does_not_blame_the_timeout(self):
        # A few seconds without a command is far below any connection_timeout worth naming.
        client = self.client(test_silence_s=5,
                             sock=BrokenSocket(recv_error=ConnectionResetError(54, "reset")))
        with self.assertRaises(ConnectionError) as cm:
            client.status()
        self.assertEqual(str(cm.exception), "MPD closed the connection")

    def test_drop_right_after_a_long_idle_wait_does_not_blame_the_timeout(self):
        # MPD restarts its connection_timeout when an idle ends, so the time spent in idle
        # is no silence, even when the wait ended by timeout and noidle.
        client = self.client(sock=ScriptedSocket([socket.timeout("idle wait over"), b"OK\n"],
                                                 recv_after_s=0.1))
        with mock.patch.object(ra, "TIMEOUT_HINT_S", 0.05):
            self.assertFalse(client.wait_player(0.1))
            with self.assertRaises(ConnectionError) as cm:
                client.status()  # EOF
        self.assertEqual(str(cm.exception), "MPD closed the connection")

    def test_drop_while_idle_does_not_blame_the_timeout(self):
        def hang_up_during_idle(conn):
            conn.sendall(b"OK MPD 0.23.5\n")
            conn.recv(4096)  # idle player
            time.sleep(0.3)  # MPD restarts while the adapter waits in idle
            conn.shutdown(socket.SHUT_WR)
            time.sleep(1)

        client = ra.MpdClient("127.0.0.1", raw_server(self, hang_up_during_idle))
        self.addCleanup(client.close)
        with self.assertRaises(ConnectionError) as cm:
            client.wait_player(None)
        self.assertIn("idle", str(cm.exception))
        self.assertNotIn(self.TIMEOUT_HINT, str(cm.exception))

    def test_reset_while_idle_does_not_blame_the_timeout(self):
        client = self.client(sock=BrokenSocket(recv_error=ConnectionResetError(54, "reset"),
                                               recv_after_s=0.2))
        with self.assertRaises(ConnectionError) as cm:
            client.wait_player(None)
        self.assertNotIn(self.TIMEOUT_HINT, str(cm.exception))
        self.assertNotIn("Errno", str(cm.exception))

    def test_noidle_into_a_dropped_connection_does_not_blame_the_timeout(self):
        client = self.client(sock=BrokenSocket(recv_error=socket.timeout("idle wait over"),
                                               recv_after_s=0.2))
        real_sendall = client.sock.sendall

        def sendall(data):
            if data.startswith(b"noidle"):
                raise BrokenPipeError(32, "pipe")
            real_sendall(data)

        client.sock.sendall = sendall
        with self.assertRaises(ConnectionError) as cm:
            client.wait_player(0.2)
        self.assertNotIn(self.TIMEOUT_HINT, str(cm.exception))


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

    def on_mpd_connected(self):
        self.connects = getattr(self, "connects", 0) + 1

    def on_mpd_state(self, state, songid=None, elapsed=None):
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

    def test_unchanged_play_after_a_reconnect_restarts_the_renderer(self):
        # MPD restarted and restored play with the same songid (ids restart at 0); the
        # stream died with it, so the owned zone's renderer is told to play again.
        ctl = RecordingController()
        follower = ra.Follower(ctl, poll_seconds=60, clock=FakeClock(), start_gate_seconds=0)
        mpd = FakeMpd([[("play", "1")], [("play", "1")]])
        mpd.close_first = True
        self.addCleanup(mpd.close)
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            run_bounded(self, follower, mpd, sleep=sleeps.append,
                        should_stop=lambda: len(ctl.calls) >= 2 or len(sleeps) > 3)
        self.assertEqual(ctl.calls, ["ensure_playing", "ensure_playing(no wake)"])
        self.assertEqual(mpd.connections, 2)

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
        self.assertEqual(ctl.calls[:3], ["mpd sent: ['pause 1']", "ensure_playing", "renderer_state"])
        self.assertEqual([c for c in mpd.commands if c.startswith("pause")], ["pause 1", "pause 0"])

    def test_song_replacing_the_preroll_silence_in_the_gate_is_paused_again(self):
        # MPD starts on the receiver's silence; the song replaces it (play, new songid)
        # while the speaker still connects. It is held too, and resumed once the speaker
        # plays, with one takeover only.
        ctl = RecordingController()
        ctl.plays = False
        follower = ra.Follower(ctl, poll_seconds=5, start_gate_seconds=5)
        mpd = FakeMpd([[("play", "1"), ("play", "2")]])
        self.addCleanup(mpd.close)
        real_state = ctl.renderer_state

        def renderer_state():
            ctl.plays = mpd.commands.count("pause 1") >= 2  # plays once the song is held
            return real_state()

        ctl.renderer_state = renderer_state
        with self.assertLogs("raumfeld_adapter", "INFO"):
            run_bounded(self, follower, mpd, sleep=lambda s: None,
                        should_stop=lambda: "pause 0" in mpd.commands)
        self.assertEqual([c for c in mpd.commands if c.startswith("pause")],
                         ["pause 1", "pause 1", "pause 0"])
        self.assertEqual(ctl.calls.count("ensure_playing"), 1)
        self.assertNotIn("ensure_playing(no wake)", ctl.calls)

    def gate_until_resumed(self, script, handover=None, resumes=1):
        """run() with the start gate against FakeMpd(script); the renderer plays as soon as
        it is asked. Returns the pause commands sent and the takeovers."""
        ctl = RecordingController()
        follower = ra.Follower(ctl, poll_seconds=5, start_gate_seconds=5)
        mpd = FakeMpd([script])
        mpd.handover_on_resume = handover
        self.addCleanup(mpd.close)
        with self.assertLogs("raumfeld_adapter", "INFO"):
            run_bounded(self, follower, mpd, sleep=lambda s: None,
                        should_stop=lambda: mpd.commands.count("pause 0") >= resumes)
        return [c for c in mpd.commands if c.startswith("pause")], ctl.calls

    def test_song_handed_over_held_during_the_takeover_needs_no_extra_pause(self):
        # The receiver saw MPD paused (our gate) and handed over held: (pause, new songid),
        # read only after the takeover returned. One takeover, no extra pause 1.
        pauses, calls = self.gate_until_resumed([("play", "1"), ("pause", "2", 0.0)])
        self.assertEqual(pauses, ["pause 1", "pause 0"])
        self.assertEqual(calls.count("ensure_playing"), 1)
        self.assertNotIn("ensure_playing(no wake)", calls)

    def test_song_handed_over_held_just_after_our_resume_is_resumed_again(self):
        # The receiver read MPD paused, then our pause 0 went out, then its list paused MPD at
        # the song: seen right after our pause 0 (the gate still resuming) or as the next
        # event (the gate already ended on the silence).
        for when in ("now", "next"):
            with self.subTest(when=when):
                pauses, calls = self.gate_until_resumed([("play", "1")], ("2", when), resumes=2)
                self.assertEqual(pauses, ["pause 1", "pause 0", "pause 0"])
                self.assertEqual(calls.count("ensure_playing"), 1)
                self.assertNotIn("ensure_playing(no wake)", calls)

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
        # Scaled down with the 0.5 s silence: the real bound (30 s) is far below MPD's 60 s.
        with mock.patch.object(ra, "TIMEOUT_HINT_S", 0.4), \
                self.assertLogs("raumfeld_adapter", "WARNING") as logs:
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
        self.assertIn("MX: 1\r\n", msg)  # answers come within a second

    def test_resends_each_second(self):
        addr = self.responder([self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml")], drop_first=1)
        ra.ssdp_discover(SPEAKER_UDN, timeout=4, addr=addr)
        self.assertEqual(len(self.received), 2)

    def test_accepts_bare_newline_replies(self):
        reply = self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml").replace("\r\n", "\n")
        addr = self.responder([reply])
        self.assertEqual(ra.ssdp_discover(SPEAKER_UDN, timeout=3, addr=addr),
                         ("127.0.0.1", "http://127.0.0.1:2/desc.xml"))

    def test_returns_at_the_first_matching_answer(self):
        addr = self.responder([self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml")])
        started = time.monotonic()
        ra.ssdp_discover(SPEAKER_UDN, timeout=3, addr=addr)
        self.assertLess(time.monotonic() - started, 0.5)

    def test_search_for_one_hosts_answer_goes_to_the_group_and_returns_at_its_first(self):
        # The Raumfeld host doesn't answer a unicast M-SEARCH; it answers a multicast one.
        addr = self.responder([self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml")])
        started = time.monotonic()
        self.assertEqual(ra.ssdp_from_host(SPEAKER_UDN, "127.0.0.1", timeout=3, group=addr),
                         ("127.0.0.1", "http://127.0.0.1:2/desc.xml"))
        self.assertLess(time.monotonic() - started, 0.5)
        msg = self.received[0]
        self.assertIn("HOST: 239.255.255.250:1900\r\n", msg)
        self.assertIn("MX: 1\r\n", msg)
        self.assertIn("ST: %s\r\n" % SPEAKER_UDN, msg)

    def test_search_for_one_hosts_answer_ignores_other_senders(self):
        addr = self.responder([self.reply(SPEAKER_UDN, "http://127.0.0.1:2/desc.xml")])
        with self.assertRaises(ra.DeviceNotFound):
            ra.ssdp_from_host(SPEAKER_UDN, "127.0.0.9", timeout=0.5, group=addr)

    def test_not_found_times_out(self):
        addr = self.responder([])
        started = time.monotonic()
        with self.assertRaises(ra.DeviceNotFound):
            ra.ssdp_discover(SPEAKER_UDN, timeout=1.5, addr=addr)
        self.assertLess(time.monotonic() - started, 3)
        self.assertEqual(len(self.received), 2)


if __name__ == "__main__":
    unittest.main()
