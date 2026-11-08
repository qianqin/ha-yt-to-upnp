"""Raumfeld adapter: makes a Teufel Raumfeld room play the receiver's MPD
httpd stream whenever MPD plays, and gives the room back as soon as someone
takes it away (power button, auto power-off, another source): it then pauses
MPD and asks the receiver to end the cast. The adapter never stops the room or
puts it into standby itself; the Raumfeld's own auto power-off does that.

Python 3.9+ standard library only. Devices are identified exclusively by UPnP
UDN (found via SSDP; a takeover always rediscovers, the frequent room check
reuses the last discovery until a request fails or the zone changes), never by IP. No volume is ever touched:
this module sends no RenderingControl requests at all.
"""

import http.client
import logging
import math
import os
import re
import socket
import sys
import time
import threading
import xml.etree.ElementTree as ET
import xml.parsers.expat
from collections import namedtuple
from html import escape as _escape
from urllib.parse import quote, unquote, urljoin, urlsplit

log = logging.getLogger("raumfeld_adapter")

SSDP_ADDR = ("239.255.255.250", 1900)
RAUMFELD_API_PORT = 47365
AVTRANSPORT = "urn:schemas-upnp-org:service:AVTransport:1"
HTTP_TIMEOUT_S = 10  # hard limit per HTTP request: connect, headers, body and redirects
MAX_BODY = 256 * 1024  # getZones, device descriptions and SOAP replies are a few KiB
MPD_MAX_LINE = 64 * 1024
UDN_RE = re.compile(r"uuid:[0-9A-Za-z-]{1,64}")
STREAM_TITLE = "YouTube Music cast"
# MPD client-to-client channel the receiver subscribes to; "release" ends its cast session.
# Fixed constants; should they ever become configurable, validate them against
# _MPD_TOKEN in Config.from_env so a bad value fails at startup, not at the first yield.
CAST_CHANNEL = "ytcast"
CAST_RELEASE_MESSAGE = "release"


class ConfigError(Exception):
    pass


class RaumfeldError(Exception):
    """The Raumfeld system answered, but not in a way we can act on."""


class DeviceNotFound(RaumfeldError):
    def __init__(self, udn):
        super().__init__("no SSDP answer for %r" % udn)


class MpdError(Exception):
    pass


# --------------------------------------------------------------------------- config

_LOG_LEVELS = ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL")


class Config(namedtuple("Config", "room_udn speaker_udn stream_url stream_mime stream_metadata "
                                  "poll_seconds mpd_host mpd_port log_level")):
    @classmethod
    def from_env(cls, env):
        required = ("RAUMFELD_ROOM_UDN", "RAUMFELD_SPEAKER_UDN", "STREAM_URL")
        missing = [k for k in required if not env.get(k, "").strip()]
        if missing:
            raise ConfigError("missing required environment variable(s): " + ", ".join(missing))

        stream_url = env["STREAM_URL"].strip()
        if not stream_url.startswith(("http://", "https://")):
            raise ConfigError("STREAM_URL must be an http(s) URL, got %r" % stream_url)

        metadata = env.get("STREAM_METADATA", "1").strip()
        if metadata not in ("0", "1"):
            raise ConfigError("STREAM_METADATA must be 0 or 1, got %r" % metadata)

        try:
            poll_seconds = float(env.get("SPEAKER_POLL_SECONDS", "5"))
        except ValueError:
            raise ConfigError("SPEAKER_POLL_SECONDS must be a number") from None
        # Below 1 s the adapter would hammer the speaker's API and SSDP.
        if not (math.isfinite(poll_seconds) and poll_seconds >= 1):
            raise ConfigError("SPEAKER_POLL_SECONDS must be at least 1")

        try:
            mpd_port = int(env.get("MPD_PORT", "6600"))
        except ValueError:
            raise ConfigError("MPD_PORT must be an integer") from None
        if not 0 < mpd_port < 65536:
            raise ConfigError("MPD_PORT must be 1-65535")

        log_level = env.get("LOG_LEVEL", "INFO").strip().upper()
        if log_level not in _LOG_LEVELS:
            raise ConfigError("LOG_LEVEL must be one of " + ", ".join(_LOG_LEVELS))

        for key in ("RAUMFELD_ROOM_UDN", "RAUMFELD_SPEAKER_UDN"):
            if not UDN_RE.fullmatch(env[key].strip()):
                raise ConfigError("%s must look like uuid:<letters, digits, dashes>, got %r"
                                  % (key, env[key]))

        return cls(
            room_udn=env["RAUMFELD_ROOM_UDN"].strip(),
            speaker_udn=env["RAUMFELD_SPEAKER_UDN"].strip(),
            stream_url=stream_url,
            stream_mime=env.get("STREAM_MIME", "audio/mpeg").strip(),
            stream_metadata=metadata == "1",
            poll_seconds=poll_seconds,
            mpd_host=env.get("MPD_HOST", "localhost").strip(),
            mpd_port=mpd_port,
            log_level=log_level,
        )


# --------------------------------------------------------------------------- pure logic

ZoneStatus = namedtuple("ZoneStatus", "transport_state current_uri")

TAKE_OVER = "take_over"
ALREADY_PLAYING = "already_playing"

_PLAYING_STATES = ("PLAYING", "TRANSITIONING")

# What a look at the room says about our zone.
OURS = "ours"  # the zone plays our stream
NOT_PLAYING = "not_playing"  # the zone holds our stream but its renderer doesn't play
STANDBY = "standby"
NO_ZONE = "no_zone"
OTHER_SOURCE = "other_source"
# Someone else has the speaker now; the adapter must give it up.
TAKEN_AWAY = (STANDBY, NO_ZONE, OTHER_SOURCE)


def is_line_in(uri):
    """Raumfeld line-in is a dlna-playsingle URI whose item id, encoded twice, names "Line In"."""
    if not uri.startswith("dlna-playsingle:"):
        return False
    return "Line In" in unquote(unquote(uri))


def decide_play(status, stream_url):
    """MPD started playing. `status` is None when the room is not active or has no zone.

    An explicit cast wins over any other source, even one reporting PLAYING: the
    Raumfeld stays on line-in after Home Assistant switches it there, and line-in
    reports PLAYING without any audio."""
    if (status is not None and status.transport_state in _PLAYING_STATES
            and status.current_uri == stream_url):
        return ALREADY_PLAYING
    return TAKE_OVER


def judge_speaker(power, zone, status, stream_url):
    """Verdict on a zone the adapter owns. `status` is None unless the room is active in a zone."""
    if zone is None:
        return NO_ZONE
    if power != "ACTIVE":
        return STANDBY
    if status.current_uri != stream_url:
        return OTHER_SOURCE
    if status.transport_state in _PLAYING_STATES:
        return OURS
    return NOT_PLAYING


def build_didl(stream_url, mime):
    return (
        '<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/" '
        'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
        '<item id="0" parentID="-1" restricted="1">'
        "<dc:title>%s</dc:title>"
        "<upnp:class>object.item.audioItem.musicTrack</upnp:class>"
        '<res protocolInfo="http-get:*:%s:*">%s</res>'
        "</item></DIDL-Lite>"
        % (_escape(STREAM_TITLE), _escape(mime), _escape(stream_url))
    )


def soap_envelope(action, args):
    body = "".join("<%s>%s</%s>" % (k, _escape(v), k) for k, v in args)
    return (
        '<?xml version="1.0" encoding="utf-8"?>'
        '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
        '<s:Body><u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>'
        % (action, AVTRANSPORT, body, action)
    )


def zone_of_room(zones_xml, room_udn):
    """UDN of the zone that contains the room (getZones reply), or None."""
    for zone in _elements(_parse_xml(zones_xml), "zone"):
        if any(room.get("udn") == room_udn for room in _elements(zone, "room")):
            udn = zone.get("udn", "")
            # The zone UDN goes into an SSDP M-SEARCH; refuse anything that isn't a plain UDN.
            if not UDN_RE.fullmatch(udn):
                raise RaumfeldError("invalid zone UDN %r" % udn)
            return udn
    return None


def room_power_state(zones_xml, room_udn):
    for room in _elements(_parse_xml(zones_xml), "room"):
        if room.get("udn") == room_udn:
            return room.get("powerState")
    return None


def check_device_url(url, host):
    """Only fetch plain http from the device that answered SSDP: no file://, no other hosts."""
    parts = urlsplit(url)
    if parts.scheme != "http" or not parts.hostname or parts.hostname != host:
        raise RaumfeldError("refusing URL %r: must be http on %r" % (url, host))
    return url


def _parse_xml(data):
    # Devices are untrusted: no DTDs, so no entity expansion tricks. A byte scan can't
    # enforce that (expat decodes UTF-16 itself), so a pyexpat pass refuses any DOCTYPE or
    # ENTITY declaration before ElementTree, whose C parser has no such hooks, builds the tree.
    if b"\x00" in data:
        raise RaumfeldError("refusing XML containing NUL bytes (UTF-16/32 or binary)")
    scanner = xml.parsers.expat.ParserCreate()
    scanner.StartDoctypeDeclHandler = _refuse_dtd
    scanner.EntityDeclHandler = _refuse_dtd
    try:
        scanner.Parse(data, True)
        return ET.fromstring(data)
    except (xml.parsers.expat.ExpatError, ET.ParseError) as e:
        raise RaumfeldError("malformed XML: %s" % e) from None


def _refuse_dtd(*_args):
    raise RaumfeldError("refusing XML with DOCTYPE/ENTITY")


def _elements(root, name):
    return [el for el in root.iter() if _local(el.tag) == name]


def avtransport_control_url(description_xml, location):
    for el in _elements(_parse_xml(description_xml), "service"):
        fields = {_local(child.tag): (child.text or "").strip() for child in el}
        if fields.get("serviceType") == AVTRANSPORT and fields.get("controlURL"):
            return urljoin(location, fields["controlURL"])
    raise RaumfeldError("no AVTransport service in device description at %r" % location)


def _local(tag):
    return tag.rsplit("}", 1)[-1]


# --------------------------------------------------------------------------- network I/O

def ssdp_discover(st, timeout=6.0, addr=SSDP_ADDR):
    """M-SEARCH for one UDN; returns (sender host, LOCATION). Re-sends every second."""
    msg = ("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n"
           'MAN: "ssdp:discover"\r\nMX: 2\r\nST: %s\r\n\r\n' % st).encode()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    try:
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
        deadline = time.monotonic() + timeout
        next_send = 0.0
        while True:
            now = time.monotonic()
            if now >= deadline:
                raise DeviceNotFound(st)
            if now >= next_send:
                sock.sendto(msg, addr)
                next_send = now + 1.0
            sock.settimeout(max(0.01, min(next_send, deadline) - now))
            try:
                data, sender = sock.recvfrom(65507)
            except socket.timeout:
                continue
            headers = _parse_headers(data.decode("utf-8", "replace"))
            if headers.get("st", "").lower() == st.lower() and headers.get("location"):
                return sender[0], headers["location"]
    finally:
        sock.close()


def _parse_headers(text):
    headers = {}
    for line in text.splitlines()[1:]:
        name, sep, value = line.partition(":")
        if sep:
            headers[name.strip().lower()] = value.strip()
    return headers


_REDIRECTS = (301, 302, 303, 307, 308)
MAX_REDIRECTS = 3


def _fetch(method, url, deadline_s, body=None, headers=None):
    """One HTTP exchange, redirects included, hard-limited to deadline_s in total.

    Only plain http; GET redirects are followed (the Raumfeld host answers 307 to
    /<uuid>/<path>) but never off the original host. Bodies above MAX_BODY are refused.
    """
    deadline = time.monotonic() + deadline_s
    host = urlsplit(url).hostname
    for _ in range(MAX_REDIRECTS + 1):
        parts = urlsplit(url)
        if parts.scheme != "http" or not parts.hostname:
            raise RaumfeldError("refusing non-http URL %r" % url)
        status, location, data = _exchange(method, url, body, headers or {}, deadline)
        if status in _REDIRECTS and method == "GET" and location:
            url = check_device_url(urljoin(url, location), host)
            continue
        if not 200 <= status < 300:
            raise RaumfeldError("HTTP %d from %r" % (status, url))
        return data
    raise RaumfeldError("too many redirects from %r" % url)


def _exchange(method, url, body, headers, deadline):
    parts = urlsplit(url)
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise RaumfeldError("%r: request deadline exceeded" % url)
    conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=remaining)
    expired = threading.Event()
    # http.client drops conn.sock after a "Connection: close" response while the body is
    # still read from it, so keep our own reference for the timer.
    sockets = []

    def expire():
        # Socket timeouts only bound single reads; shutting the socket down bounds the
        # whole exchange, including a device that trickles headers or body bytes.
        expired.set()
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    timer = threading.Timer(remaining, expire)
    timer.daemon = True
    timer.start()
    resp = None
    try:
        conn.connect()
        sockets.append(conn.sock)
        if expired.is_set():
            raise socket.timeout("deadline passed while connecting")
        path = (parts.path or "/") + ("?" + parts.query if parts.query else "")
        conn.request(method, path, body=body, headers=headers)
        resp = conn.getresponse()
        data = b""
        while True:
            chunk = resp.read1(min(65536, MAX_BODY + 1 - len(data)))
            if not chunk:
                break
            data += chunk
            if len(data) > MAX_BODY:
                raise RaumfeldError("response from %r exceeds %d bytes" % (url, MAX_BODY))
        if expired.is_set():
            raise socket.timeout("deadline passed while reading")
        return resp.status, resp.getheader("Location"), data
    except (OSError, http.client.HTTPException):
        if expired.is_set():
            raise RaumfeldError("%r took longer than the request deadline" % url) from None
        raise
    finally:
        timer.cancel()
        if resp is not None:
            resp.close()  # with Connection: close the response owns the socket
        conn.close()


def http_get(url, deadline_s=HTTP_TIMEOUT_S):
    return _fetch("GET", url, deadline_s)


def soap_call(control_url, action, args):
    body = soap_envelope(action, [("InstanceID", "0")] + list(args)).encode("utf-8")
    headers = {"Content-Type": 'text/xml; charset="utf-8"',
               "SOAPACTION": '"%s#%s"' % (AVTRANSPORT, action)}
    root = _parse_xml(_fetch("POST", control_url, HTTP_TIMEOUT_S, body, headers))
    return {_local(el.tag): (el.text or "") for el in root.iter()}


# What one inspection of the room found; control/status are None unless the room is active in a zone.
_Room = namedtuple("_Room", "api zone power control status")


class Controller:
    """Drives the Raumfeld room. Taking over rediscovers everything via SSDP, because
    host IPs change and the zone renderer comes and goes; the frequent `check` reuses
    the last discovery until it fails or the zone changes."""

    def __init__(self, config, discover, api_port=RAUMFELD_API_PORT, sleep=time.sleep,
                 zone_wait_s=10.0, zone_poll_s=0.5):
        self.config = config
        self.discover = discover
        self.api_port = api_port
        self.sleep = sleep
        self.zone_polls = int(zone_wait_s / zone_poll_s)
        self.zone_poll_s = zone_poll_s
        self._api = None
        self._control = None  # (zone UDN, AVTransport control URL)

    def ensure_playing(self, wake=True):
        """Make the room play our stream; returns the decision taken. With wake=False
        (a track change on a zone we already own) a room someone took away is left
        alone and its verdict returned instead."""
        room = self._inspect()
        if not wake:
            verdict = judge_speaker(room.power, room.zone, room.status, self.config.stream_url)
            if verdict in TAKEN_AWAY:
                return verdict
        decision = decide_play(room.status, self.config.stream_url)
        if decision == ALREADY_PLAYING:
            log.debug("zone %r already plays the stream", room.zone)
            return decision
        if room.status and room.status.current_uri not in ("", self.config.stream_url):
            source = "line-in" if is_line_in(room.status.current_uri) else "another source"
            log.info("zone %r was on %s; the cast takes over", room.zone, source)

        room_param = quote(self.config.room_udn, safe="")
        if wake:
            http_get(room.api + "leaveStandby?roomUDN=" + room_param)
        zone = room.zone or self._connect_zone(room.api, room_param)
        control = room.control or self._zone_control(zone, reuse=False)
        metadata = (build_didl(self.config.stream_url, self.config.stream_mime)
                    if self.config.stream_metadata else "")
        soap_call(control, "SetAVTransportURI",
                  [("CurrentURI", self.config.stream_url), ("CurrentURIMetaData", metadata)])
        soap_call(control, "Play", [("Speed", "1")])
        log.info("zone %r now plays %s", zone, self.config.stream_url)
        return decision

    def check(self):
        """Read-only look at the room: the judge_speaker verdict."""
        try:
            room = self._inspect(reuse=True)
        except Exception:
            # The device may have moved; find it again next time.
            self._api = self._control = None
            raise
        return judge_speaker(room.power, room.zone, room.status, self.config.stream_url)

    def _inspect(self, reuse=False):
        if not (reuse and self._api):
            host, _location = self.discover(self.config.speaker_udn)
            self._api = "http://%s:%d/" % (host, self.api_port)
        api = self._api
        zones = http_get(api + "getZones")
        zone = zone_of_room(zones, self.config.room_udn)
        power = room_power_state(zones, self.config.room_udn)
        # A room in standby (or without zone) plays nothing; don't query a sleeping renderer.
        if zone is None or power != "ACTIVE":
            return _Room(api, zone, power, None, None)
        control = self._zone_control(zone, reuse)
        info = soap_call(control, "GetTransportInfo", [])
        media = soap_call(control, "GetMediaInfo", [])
        status = ZoneStatus(info.get("CurrentTransportState", ""), media.get("CurrentURI", ""))
        return _Room(api, zone, power, control, status)

    def _zone_control(self, zone, reuse):
        if not (reuse and self._control and self._control[0] == zone):
            self._control = (zone, self._control_url(zone))
        return self._control[1]

    def _connect_zone(self, api, room):
        # connectRoomToZone makes a brand-new zone each time, so call it exactly once.
        http_get(api + "connectRoomToZone?roomUDN=" + room)
        for _ in range(self.zone_polls):
            zone = zone_of_room(http_get(api + "getZones"), self.config.room_udn)
            if zone is not None:
                return zone
            self.sleep(self.zone_poll_s)
        raise RaumfeldError("room did not appear in a zone after connectRoomToZone")

    def _control_url(self, zone):
        host, location = self.discover(zone)
        check_device_url(location, host)
        return check_device_url(avtransport_control_url(http_get(location), location), host)


# --------------------------------------------------------------------------- MPD

_MPD_TOKEN = re.compile(r"[A-Za-z0-9_.:-]{1,64}")


class MpdClient:
    """Minimal MPD protocol client: `status`, `idle player`, `pause 1` and `sendmessage`.
    Never `stop`: the receiver takes an MPD stop for the end of the track and advances."""

    def __init__(self, host, port, timeout=HTTP_TIMEOUT_S):
        self.timeout = timeout
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
        self.buf = b""
        try:
            banner = self._readline()
            if not banner.startswith("OK MPD "):
                raise MpdError("not an MPD server: %r" % banner[:40])
        except BaseException:
            self.close()
            raise

    def close(self):
        self.sock.close()

    def state(self):
        self._send("status")
        for line in self._response():
            key, _, value = line.partition(": ")
            if key == "state":
                return value
        raise MpdError("status without state")

    def pause(self):
        self._command("pause 1")

    def send_message(self, channel, text):
        for token in (channel, text):
            if not _MPD_TOKEN.fullmatch(token):
                raise ValueError("refusing MPD message token %r" % token)
        self._command("sendmessage %s %s" % (channel, text))

    def wait_player(self, timeout):
        """Block until the player changes (True) or `timeout` seconds pass (False;
        None = forever)."""
        if timeout is not None and timeout <= 0:
            raise ValueError("idle timeout must be positive or None, got %r" % timeout)
        self._send("idle player")
        self.sock.settimeout(timeout)
        try:
            self._response()
            return True
        except socket.timeout:
            self.sock.settimeout(self.timeout)
            self._send("noidle")
            # MPD may have reported a change just before it saw noidle.
            return bool(self._response())
        finally:
            self.sock.settimeout(self.timeout)

    def _command(self, command):
        self._send(command)
        self._response()

    def _send(self, command):
        self.sock.sendall(command.encode() + b"\n")

    def _response(self):
        lines = []
        while True:
            line = self._readline()
            if line == "OK":
                return lines
            if line.startswith("ACK "):
                raise MpdError(line)
            lines.append(line)

    def _readline(self):
        # Own buffer instead of makefile(): a recv timeout must not lose buffered data.
        while b"\n" not in self.buf:
            if len(self.buf) > MPD_MAX_LINE:
                raise MpdError("MPD line longer than %d bytes" % MPD_MAX_LINE)
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("MPD closed the connection")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")


# --------------------------------------------------------------------------- orchestration

# Everything a misbehaving network or device can cause; the follow loop must survive it.
_ERRORS = (OSError, ValueError, http.client.HTTPException, RaumfeldError, ET.ParseError)


# Delay before re-trying a failed takeover while MPD keeps playing.
PLAY_RETRY_S = 30
# Shortest idle wait: a zero timeout would turn the MPD socket non-blocking.
MIN_WAIT_S = 0.1
DEFAULT_POLL_S = 5
# Consecutive polls that may find our renderer stopped while MPD plays (a stream
# reconnect between songs) before the speaker counts as taken away.
STOPPED_GRACE_POLLS = 2

# What the Follower asks run() to do on the MPD connection.
PAUSE_PLAYER = "pause"
RELEASE_CAST = "release_cast"

_PLAY = "ensure_playing"
_POLL = "check"

_REASONS = {
    STANDBY: "room went to standby",
    NO_ZONE: "room left its zone",
    OTHER_SOURCE: "zone switched to another source",
    NOT_PLAYING: "zone renderer stays stopped",
}


class Follower:
    """Turns the MPD player state stream into Controller actions. At most one action is
    pending at a time: the retry of a failed takeover, or the next look at the room.

    While it owns the zone it polls the room, whatever MPD's state. When someone takes
    the speaker away while MPD plays or is paused, it yields: it asks run() to pause MPD
    (if playing) and to tell the receiver to end the cast, and leaves the room alone
    until the user resumes (MPD pause -> play) or the receiver ends the session
    (pause -> stop). If the room goes away while MPD is stopped there is no cast to
    end: it just lets go (no pause, no release) and the next play wakes the room."""

    def __init__(self, controller, poll_seconds=DEFAULT_POLL_S, clock=time.monotonic):
        self.controller = controller
        self.poll_seconds = poll_seconds
        self.clock = clock
        self.pending = None
        self.deadline = None
        self.mpd_state = None
        self.owned = False
        self.yielded = False
        self.wake = True
        self.stopped_polls = 0
        self._requests = []

    def on_mpd_state(self, state):
        previous, self.mpd_state = self.mpd_state, state
        self.stopped_polls = 0
        if self.yielded:
            if previous != "pause" or state == "pause":
                if state == "play":
                    # A song change that raced our pause: keep the phone paused.
                    log.info("speaker was taken away; pausing MPD again")
                    if PAUSE_PLAYER not in self._requests:
                        self._requests.append(PAUSE_PLAYER)
                return
            # MPD left the pause on the user's behalf: resumed, or the session ended.
            self.yielded = False
        if state == "play":
            # Only a user resume may wake a zone we own; a song change must not.
            self.wake = previous == "pause" or not self.owned
            self._attempt(_PLAY)
        elif self.owned:
            # Keep watching a paused or stopped zone: the Raumfeld's auto power-off may end
            # a pause (that ends the cast), and after a stop it tells us the room is no
            # longer ours, so the next cast may wake it.
            self._schedule(_POLL)
        else:
            self.pending = None

    def take_mpd_requests(self):
        requests, self._requests = self._requests, []
        return requests

    def requeue_mpd_requests(self, requests):
        """Put back requests run() could not serve (lost connection), ahead of newer ones."""
        self._requests = list(requests) + [r for r in self._requests if r not in requests]

    def seconds_until_deadline(self):
        if self.pending is None:
            return None
        return max(0.0, self.deadline - self.clock())

    def tick(self):
        if self.pending is not None and self.clock() >= self.deadline:
            self._attempt(self.pending)

    def _attempt(self, action):
        self.pending = None
        try:
            if action == _PLAY:
                self._take_over()
            else:
                self._poll()
        except _ERRORS as e:
            # %r: the message may carry device-supplied text (e.g. an HTTP reason phrase).
            log.warning("%s failed: %r", action, e)
            # Don't give up on a missed takeover; a failed look is not "taken away".
            self._schedule(action)

    def _take_over(self):
        decision = self.controller.ensure_playing(wake=self.wake)
        if decision in TAKEN_AWAY:
            self._yield(decision)
        else:
            self.owned = True
            self._schedule(_POLL)

    def _poll(self):
        verdict = self.controller.check()
        if verdict in TAKEN_AWAY and self.mpd_state == "stop":
            # No cast to end; just let go, so the next cast wakes the room as usual.
            log.info("%s while MPD is stopped; the next cast takes the room again",
                     _REASONS[verdict])
            self.owned = False
            return
        if verdict in TAKEN_AWAY:
            return self._yield(verdict)
        if verdict == NOT_PLAYING and self.mpd_state == "play":
            self.stopped_polls += 1
            if self.stopped_polls > STOPPED_GRACE_POLLS:
                return self._yield(verdict)
        elif verdict == OURS:
            self.stopped_polls = 0
        self._schedule(_POLL)

    def _yield(self, verdict):
        log.info("speaker taken away (%s): ending the cast; not taking the room back "
                 "until playback is resumed from the phone", _REASONS[verdict])
        if self.mpd_state == "play":
            # Before the message, so the stream stops even if the receiver ignores it.
            self._requests.append(PAUSE_PLAYER)
        self._requests.append(RELEASE_CAST)
        self.yielded = True
        self.owned = False

    def _schedule(self, action):
        self.pending = action
        self.deadline = self.clock() + (PLAY_RETRY_S if action == _PLAY else self.poll_seconds)


def _serve_requests(follower, mpd):
    requests = follower.take_mpd_requests()
    for i, request in enumerate(requests):
        try:
            if request == PAUSE_PLAYER:
                mpd.pause()
                # Note the pause before the message goes out: a receiver that ends the
                # session right away would otherwise fold our pause and its stop into one
                # MPD event.
                follower.on_mpd_state(mpd.state())
            else:
                _send_release(mpd)
        except BaseException:
            # The connection is gone; what is left must still go out after the reconnect.
            follower.requeue_mpd_requests(requests[i:])
            raise


def _send_release(mpd):
    try:
        mpd.send_message(CAST_CHANNEL, CAST_RELEASE_MESSAGE)
        log.info("asked the receiver to end the cast (%s %s)", CAST_CHANNEL, CAST_RELEASE_MESSAGE)
    except MpdError as e:
        # e.g. an older receiver that doesn't subscribe: MPD answers "nobody is subscribed".
        log.warning("could not send %r on MPD channel %r: %s",
                    CAST_RELEASE_MESSAGE, CAST_CHANNEL, e)


def run(follower, connect_mpd, sleep=time.sleep, should_stop=lambda: False):
    backoff = 1
    while not should_stop():
        try:
            mpd = connect_mpd()
        except (OSError, MpdError) as e:
            log.warning("cannot connect to MPD: %s; retrying in %ss", e, backoff)
            sleep(backoff)
            backoff = min(backoff * 2, 30)
            continue
        backoff = 1
        try:
            follower.on_mpd_state(mpd.state())
            # Tick right away too: a deadline may have passed while MPD was unreachable.
            follower.tick()
            _serve_requests(follower, mpd)
            while not should_stop():
                wait = follower.seconds_until_deadline()
                # Only a real player change is an MPD event; a timeout just means "tick".
                if mpd.wait_player(None if wait is None else max(wait, MIN_WAIT_S)):
                    follower.on_mpd_state(mpd.state())
                follower.tick()
                # Outside `idle`: the connection is free for our own commands.
                _serve_requests(follower, mpd)
        except (OSError, MpdError) as e:
            log.warning("lost MPD connection: %s; reconnecting in %ss", e, backoff)
            sleep(backoff)
            backoff = min(backoff * 2, 30)
        finally:
            mpd.close()


def main(env=None):
    env = os.environ if env is None else env
    try:
        config = Config.from_env(env)
    except ConfigError as e:
        print("raumfeld-adapter: configuration error: %s" % e, file=sys.stderr)
        return 2
    logging.basicConfig(level=config.log_level,
                        format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    log.info("following MPD at %s:%d for room %s", config.mpd_host, config.mpd_port, config.room_udn)
    if "IDLE_MINUTES" in env:
        log.warning("IDLE_MINUTES is ignored: the Raumfeld's own auto power-off puts the room "
                    "into standby")
    controller = Controller(config, ssdp_discover)
    follower = Follower(controller, config.poll_seconds)
    try:
        run(follower, lambda: MpdClient(config.mpd_host, config.mpd_port))
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
