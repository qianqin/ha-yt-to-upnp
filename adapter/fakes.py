"""Test doubles: a fake Raumfeld host (HTTP API, device description, AVTransport
SOAP endpoint), a fake MPD TCP server and a fake SSDP resolver.

All identifiers are obviously fake placeholders; servers bind to 127.0.0.1 on
ephemeral ports.
"""

import socket
import threading
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

ROOM_UDN = "uuid:00000000-0000-0000-0000-000000000001"
SPEAKER_UDN = "uuid:00000000-0000-0000-0000-000000000002"
ZONE_UDN = "uuid:00000000-0000-0000-0000-000000000003"
NEW_ZONE_UDN = "uuid:00000000-0000-0000-0000-000000000004"
OTHER_ROOM_UDN = "uuid:00000000-0000-0000-0000-000000000005"
OTHER_ZONE_UDN = "uuid:00000000-0000-0000-0000-000000000006"
STREAM_URL = "http://receiver.invalid:8000/stream.mp3?a=1&b=2"
HOST_PREFIX = "/00000000-0000-0000-0000-0000000000aa"
LINE_IN_URI = (
    "dlna-playsingle://uuid%3A00000000-0000-0000-0000-000000000002"
    "?sid=urn%3Aupnp-org%3AserviceId%3AContentDirectory"
    "&iid=0%2FLine%2520In%2F00000000-0000-0000-0000-000000000002"
)

AVT_NS = "urn:schemas-upnp-org:service:AVTransport:1"


class FakeRaumfeld:
    """Records every request; models zones, room power state and the zone renderer."""

    def __init__(self):
        self.requests = []  # (method, path, soap_action or None, body)
        self.power = "MANUAL_STANDBY"
        self.zone_udn = ZONE_UDN  # None = room not in any zone
        self.polls_until_new_zone = 0
        self.transport_state = "STOPPED"
        self.current_uri = ""
        # A renderer that dropped the stream: Play alone leaves it TRANSITIONING; Stop clears it.
        self.stuck = False
        self.fail_paths = set()  # substrings of paths that answer 500
        self.control_url = "AVTransport/ctrl"  # as advertised in the device description
        self.redirect_base = HOST_PREFIX  # where the API's 307 points
        self.lock = threading.Lock()
        self._pending_new_zone = None
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, args=(0.05,), daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    @property
    def base(self):
        return "http://127.0.0.1:%d" % self.port

    def api_calls(self):
        return [urlsplit(p).path.split("/")[-1] for m, p, a, b in self.requests
                if m == "GET" and p.startswith(HOST_PREFIX)]

    def soap_actions(self):
        return [a for m, p, a, b in self.requests if a]

    def soap_body(self, action):
        for m, p, a, b in self.requests:
            if a == action:
                return b
        raise AssertionError("no SOAP call %s" % action)

    def zones_xml(self):
        zone = ""
        if self.zone_udn:
            zone = (
                "<zone udn='%s'><room udn='%s' name='Room A' powerState='%s'>"
                "<renderer udn='%s' name='Speaker A'/></room></zone>"
                % (self.zone_udn, ROOM_UDN, self.power, SPEAKER_UDN)
            )
            unassigned = ""
        else:
            unassigned = (
                "<unassignedRooms><room udn='%s' name='Room A' powerState='%s'/>"
                "</unassignedRooms>" % (ROOM_UDN, self.power)
            )
        other = (
            "<zone udn='%s'><room udn='%s' name='Room B' powerState='ACTIVE'/></zone>"
            % (OTHER_ZONE_UDN, OTHER_ROOM_UDN)
        )
        return ("<?xml version='1.0' encoding='utf-8'?><zoneConfig><zones>%s%s</zones>%s"
                "</zoneConfig>" % (other, zone, unassigned))

    def device_xml(self):
        return (
            "<?xml version='1.0'?><root xmlns='urn:schemas-upnp-org:device-1-0'><device>"
            "<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>"
            "<UDN>%s</UDN><serviceList>"
            "<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1"
            "</serviceType><controlURL>/RenderingControl/ctrl</controlURL></service>"
            "<service><serviceType>%s</serviceType>"
            "<controlURL>%s</controlURL></service>"
            "</serviceList></device></root>" % (self.zone_udn, AVT_NS, self.control_url)
        )

    def _api(self, name, query):
        room = query.get("roomUDN", [None])[0]
        if name == "getZones":
            if self._pending_new_zone is not None:
                if self.polls_until_new_zone <= 0:
                    self.zone_udn, self._pending_new_zone = self._pending_new_zone, None
                else:
                    self.polls_until_new_zone -= 1
            return self.zones_xml()
        assert room == ROOM_UDN, "unexpected roomUDN %r" % room
        if name == "leaveStandby":
            self.power = "ACTIVE"
        elif name == "enterManualStandby":
            self.power = "MANUAL_STANDBY"
        elif name == "connectRoomToZone":
            self._pending_new_zone = NEW_ZONE_UDN
        else:
            raise AssertionError("unknown API " + name)
        return ""

    def _soap(self, action, body):
        root = ET.fromstring(body)
        args = {el.tag: (el.text or "") for el in root.iter() if "}" not in el.tag}
        assert args.get("InstanceID") == "0"
        out = ""
        if action == "GetTransportInfo":
            out = "<CurrentTransportState>%s</CurrentTransportState>" % self.transport_state
        elif action == "GetMediaInfo":
            out = "<CurrentURI>%s</CurrentURI>" % _xml_escape(self.current_uri)
        elif action == "SetAVTransportURI":
            self.current_uri = args["CurrentURI"]
            self.transport_state = "STOPPED"
        elif action == "Play":
            assert args.get("Speed") == "1"
            self.transport_state = "TRANSITIONING" if self.stuck else "PLAYING"
        elif action == "Stop":
            self.transport_state = "STOPPED"
            self.stuck = False
        else:
            raise AssertionError("unknown SOAP action " + action)
        return (
            "<?xml version='1.0'?><s:Envelope xmlns:s='http://schemas.xmlsoap.org/soap/envelope/'>"
            "<s:Body><u:%sResponse xmlns:u='%s'>%s</u:%sResponse></s:Body></s:Envelope>"
            % (action, AVT_NS, out, action)
        )

    def _handler(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _send(self, code, body="", headers=()):
                data = body.encode("utf-8")
                self.send_response(code)
                for k, v in headers:
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                with fake.lock:
                    fake.requests.append(("GET", self.path, None, ""))
                    url = urlsplit(self.path)
                    if any(f in url.path for f in fake.fail_paths):
                        return self._send(500, "boom")
                    if url.path == "/zone.xml":
                        return self._send(200, fake.device_xml())
                    if not url.path.startswith(HOST_PREFIX):
                        # The real host redirects /<api> to /<uuid>/<api>.
                        return self._send(307, "", [("Location", fake.redirect_base + self.path)])
                    name = url.path.rsplit("/", 1)[-1]
                    return self._send(200, fake._api(name, parse_qs(url.query)))

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length).decode("utf-8")
                soap_action = self.headers.get("SOAPACTION", "")
                with fake.lock:
                    fake.requests.append(("POST", self.path, soap_action.strip('"').split("#")[-1], body))
                    if any(f in self.path for f in fake.fail_paths):
                        return self._send(500, "boom")
                    assert self.path == "/AVTransport/ctrl", self.path
                    assert soap_action == '"%s#%s"' % (AVT_NS, fake.requests[-1][2]), soap_action
                    return self._send(200, fake._soap(fake.requests[-1][2], body),
                                      [("Content-Type", 'text/xml; charset="utf-8"')])

        return Handler


def _xml_escape(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class FakeDiscovery:
    """Stands in for SSDP: maps a search target (UDN) to (host, LOCATION)."""

    def __init__(self, raumfeld):
        self.raumfeld = raumfeld
        self.searches = []
        self.speaker_present = True
        self.zone_location = None  # override the zone renderer's LOCATION

    def __call__(self, st):
        from raumfeld_adapter import DeviceNotFound

        self.searches.append(st)
        if st == SPEAKER_UDN and self.speaker_present:
            return "127.0.0.1", self.raumfeld.base + "/speaker.xml"
        if st == self.raumfeld.zone_udn:
            return "127.0.0.1", self.zone_location or self.raumfeld.base + "/zone.xml"
        raise DeviceNotFound(st)


class FakeMpd:
    """A scripted MPD server. Each accepted connection consumes one script:
    a list of player states; the first answers the initial `status`, each
    following one is announced via `changed: player` to the next `idle player`.
    When a script is exhausted the server holds `idle` open until `noidle`
    (or closes the connection if close_after is set). `pause 1` pauses the
    player and is announced to the next `idle player`. `sendmessage` succeeds while
    `message_subscribers` is set; otherwise MPD's "nobody is subscribed" ACK."""

    def __init__(self, scripts, close_after=False):
        self.scripts = list(scripts)
        self.close_after = close_after
        self.close_first = False  # drop the first connection at its first `idle player`
        self.message_subscribers = True
        self.stop_on_message = False  # a receiver that ends its session on `release`
        self.drop_on_first_pause = False  # lose the connection on the first `pause 1`
        self.connections = 0
        self.commands = []
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(5)
        self.port = self.sock.getsockname()[1]
        self._stop = False
        self._conns = []
        threading.Thread(target=self._serve, daemon=True).start()

    def close(self):
        """Also hangs up open sessions, so a client blocked in `idle` wakes up."""
        self._stop = True
        self.sock.close()
        for conn in list(self._conns):
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def _serve(self):
        while not self._stop:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            script = self.scripts.pop(0) if self.scripts else ["stop"]
            self.connections += 1
            self._conns.append(conn)
            close = self.close_after or (self.close_first and self.connections == 1)
            threading.Thread(target=self._session, args=(conn, list(script), close),
                             daemon=True).start()

    def _session(self, conn, states, close_after):
        f = conn.makefile("rwb", buffering=0)
        try:
            f.write(b"OK MPD 0.23.5\n")
            state = states.pop(0)
            idling = False
            paused_by_client = False
            while True:
                line = f.readline()
                if not line:
                    return
                cmd = line.decode().strip()
                self.commands.append(cmd)
                if cmd == "status":
                    f.write(("volume: -1\nrepeat: 0\nstate: %s\nOK\n" % state).encode())
                elif cmd == "pause 1" and self.drop_on_first_pause:
                    self.drop_on_first_pause = False
                    return
                elif cmd == "pause 1":
                    state, paused_by_client = "pause", True
                    f.write(b"OK\n")
                elif cmd.startswith("sendmessage ") and self.message_subscribers:
                    if self.stop_on_message:
                        state, paused_by_client = "stop", True
                    f.write(b"OK\n")
                elif cmd.startswith("sendmessage "):
                    f.write(b"ACK [50@0] {sendmessage} nobody is subscribed to this channel\n")
                elif cmd == "idle player":
                    if paused_by_client:
                        paused_by_client = False
                        f.write(b"changed: player\nOK\n")
                    elif states:
                        state = states.pop(0)
                        f.write(b"changed: player\nOK\n")
                    elif close_after:
                        return
                    else:
                        idling = True
                elif cmd == "noidle" and idling:
                    idling = False
                    f.write(b"OK\n")
                else:
                    f.write(b"ACK [5@0] {} unknown command\n")
        finally:
            conn.close()


def run_bounded(test, follower, mpd, should_stop, sleep, limit_s=5):
    """Run raumfeld_adapter.run() against a live FakeMpd in a daemon thread. A regression
    that leaves run() blocked in MPD `idle` (or spinning) fails the test after limit_s,
    listing the MPD commands seen, instead of hanging the suite. Exceptions raised inside
    run() are re-raised here."""
    import raumfeld_adapter as ra

    given_up = threading.Event()
    errors = []

    def target():
        try:
            ra.run(follower, lambda: ra.MpdClient("127.0.0.1", mpd.port), sleep=sleep,
                   should_stop=lambda: given_up.is_set() or should_stop())
        except BaseException as e:  # surfaced in the test thread below
            errors.append(e)

    runner = threading.Thread(target=target, daemon=True)
    runner.start()
    runner.join(limit_s)
    if runner.is_alive():
        given_up.set()
        mpd.close()  # wakes run() out of `idle`; it then sees should_stop
        runner.join(2)
        test.fail("run() still running after %ss; MPD commands: %r" % (limit_s, mpd.commands))
    if errors:
        raise errors[0]
