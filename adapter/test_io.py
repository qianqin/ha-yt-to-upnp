"""I/O edges: MPD client and main loop against a fake MPD server, SSDP against a local responder."""

import socket
import threading
import time
import unittest

import raumfeld_adapter as ra
from fakes import SPEAKER_UDN, FakeMpd
from test_logic import FakeClock, RecordingController


class MpdClientTest(unittest.TestCase):
    def test_status_and_idle(self):
        mpd = FakeMpd([["stop", "play"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        self.assertEqual(client.state(), "stop")
        client.wait_player(None)
        self.assertEqual(client.state(), "play")
        self.assertEqual(mpd.commands, ["status", "idle player", "status"])

    def test_idle_timeout_sends_noidle_and_connection_stays_usable(self):
        mpd = FakeMpd([["pause"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        started = time.monotonic()
        client.wait_player(0.2)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(client.state(), "pause")
        self.assertEqual(mpd.commands, ["idle player", "noidle", "status"])

    def test_non_positive_idle_timeout_is_rejected(self):
        mpd = FakeMpd([["stop"]])
        self.addCleanup(mpd.close)
        client = ra.MpdClient("127.0.0.1", mpd.port)
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.wait_player(0.0)
        self.assertEqual(client.state(), "stop")

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


class RecordingFollower:
    def __init__(self, stop_after):
        self.states = []
        self.ticks = 0
        self.stop_after = stop_after

    def on_mpd_state(self, state):
        self.states.append(state)

    def tick(self):
        self.ticks += 1

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
            ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port),
                   sleep=sleeps.append, should_stop=follower.done)
        self.assertEqual(follower.states, ["stop", "play", "pause"])
        self.assertEqual(follower.ticks, 3)  # after each connect and each wake-up
        self.assertEqual(sleeps, [1])

    def test_backoff_resets_after_successful_reconnect(self):
        mpd = FakeMpd([["stop"], ["stop"], ["stop"]], close_after=True)
        self.addCleanup(mpd.close)
        follower = RecordingFollower(stop_after=3)
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port),
                   sleep=sleeps.append, should_stop=follower.done)
        self.assertEqual(sleeps, [1, 1])

    def test_deadline_passed_during_disconnect_releases_without_reconnect_loop(self):
        clock = FakeClock()
        ctl = RecordingController()
        follower = ra.Follower(ctl, idle_seconds=300, clock=clock)
        # Connection 1 reports stop then drops; connection 2 stays open in idle.
        mpd = FakeMpd([["stop"], ["stop"]], close_after=False)
        mpd.close_first = True
        self.addCleanup(mpd.close)
        sleeps = []

        def sleep(seconds):
            sleeps.append(seconds)
            clock.now += 400  # the outage outlasts the idle timeout

        with self.assertLogs("raumfeld_adapter", "WARNING"):
            ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port), sleep=sleep,
                   should_stop=lambda: bool(ctl.calls) or len(sleeps) > 3)
        self.assertEqual(ctl.calls, ["release"])
        self.assertEqual(sleeps, [1])

    def test_past_deadline_wait_is_clamped_positive(self):
        clock = FakeClock()
        ctl = RecordingController()
        ctl.fail = OSError("speaker unreachable")
        follower = ra.Follower(ctl, idle_seconds=300, clock=clock)
        mpd = FakeMpd([["stop"]])
        self.addCleanup(mpd.close)
        follower.on_mpd_state("stop")
        clock.now += 1000  # deadline already passed before MPD is even reached
        ticks = []
        real_tick = follower.tick

        def tick():
            ticks.append(1)
            real_tick()  # release fails and re-arms ...
            clock.now += 1000  # ... but the next deadline is already passed again

        follower.tick = tick
        sleeps = []
        with self.assertLogs("raumfeld_adapter", "WARNING"):
            ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port), sleep=sleeps.append,
                   should_stop=lambda: len(ticks) >= 3 or bool(sleeps))
        self.assertEqual(sleeps, [])
        self.assertEqual(mpd.commands.count("noidle"), 2)

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

    def test_not_found_times_out(self):
        addr = self.responder([])
        started = time.monotonic()
        with self.assertRaises(ra.DeviceNotFound):
            ra.ssdp_discover(SPEAKER_UDN, timeout=1.2, addr=addr)
        self.assertLess(time.monotonic() - started, 3)
        self.assertEqual(len(self.received), 2)


if __name__ == "__main__":
    unittest.main()
