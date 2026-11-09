"""Raumfeld adapter: makes a Teufel Raumfeld room play the receiver's MPD
httpd stream whenever MPD plays, and gives the room back as soon as someone
takes it away (power button, auto power-off, another source): it then pauses
MPD and asks the receiver to end the cast. The adapter never puts the room into
standby itself (the Raumfeld's own auto power-off does that) and only stops the
zone renderer to restart it (Stop, Play) when it hangs in TRANSITIONING.

Python 3.9+ standard library only. Devices are identified exclusively by UPnP
UDN (found via SSDP; a takeover always rediscovers, the frequent room check
reuses the last discovery until a request fails or the zone changes), never by IP. No volume is ever touched:
this module sends no RenderingControl requests at all.
"""

import errno
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
# After leaveStandby the room is asked every WAKE_POLL_S whether it is awake (ACTIVE, in
# its zone), for at most WAKE_WAIT_S in all, slow answers included; a speaker still
# finishing its standby accepts the takeover and drops it.
WAKE_WAIT_S = 10
WAKE_POLL_S = 0.5
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


def _seconds(env, key, default, minimum, maximum=math.inf):
    try:
        value = float(env.get(key, default))
    except ValueError:
        raise ConfigError("%s must be a number" % key) from None
    if not (math.isfinite(value) and minimum <= value <= maximum):
        if maximum == math.inf:
            raise ConfigError("%s must be at least %g" % (key, minimum))
        raise ConfigError("%s must be %g-%g" % (key, minimum, maximum))
    return value


class Config(namedtuple("Config", "room_udn speaker_udn stream_url stream_mime stream_metadata "
                                  "poll_seconds stuck_seconds start_gate_seconds mpd_host mpd_port "
                                  "log_level")):
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

        # Below 1 s the adapter would hammer the speaker's API and SSDP.
        poll_seconds = _seconds(env, "SPEAKER_POLL_SECONDS", "5", 1)
        # Beyond a few minutes of silence a restart comes too late to be of use.
        stuck_seconds = _seconds(env, "STUCK_SECONDS", str(STUCK_S), 1, 300)
        # Holding the song any longer only leaves the phone standing still. (The gate goes
        # back to idle every GATE_POLL_S, so its length never counts toward MPD's
        # connection_timeout.)
        start_gate_seconds = _seconds(env, "START_GATE_SECONDS", str(START_GATE_S), 0,
                                      START_GATE_MAX_S)

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
            stuck_seconds=stuck_seconds,
            start_gate_seconds=start_gate_seconds,
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
# The zone holds our stream and is about to play it -- or, if this lasts, has dropped the
# stream and hangs (seen live after a speaker request timed out).
TRANSITIONING = "transitioning"
NOT_PLAYING = "not_playing"  # the zone holds our stream but its renderer doesn't play
STANDBY = "standby"
NO_ZONE = "no_zone"
OTHER_SOURCE = "other_source"
# Someone else has the speaker now; the adapter must give it up.
TAKEN_AWAY = (STANDBY, NO_ZONE, OTHER_SOURCE)
# Appended by Controller.renderer_state when the renderer holds a URI other than ours: the
# one it held when the last takeover set up our stream (that takeover was dropped), or
# another one (someone chose it since).
PREVIOUS_SOURCE = " (source before the takeover)"
ANOTHER_SOURCE = " (another source)"
# Appended instead when the renderer holds no URI at all: nothing anyone chose.
NOTHING_LOADED = " (nothing loaded)"
# The longest transport state taken from a device into a log line.
MAX_STATE_LEN = 64


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


def plays_another_source(status, stream_url):
    """The zone renderer plays (or is about to play) something other than our stream. An
    empty URI is nothing loaded, whatever the transport state says: no source."""
    return (status is not None and status.transport_state in _PLAYING_STATES
            and status.current_uri not in ("", stream_url))


def judge_speaker(power, zone, status, stream_url):
    """Verdict on a zone the adapter owns. `status` is None unless the room is active in a zone."""
    if zone is None:
        return NO_ZONE
    if power != "ACTIVE":
        return STANDBY
    if status.current_uri != stream_url:
        return OTHER_SOURCE
    if status.transport_state == "PLAYING":
        return OURS
    if status.transport_state == "TRANSITIONING":
        return TRANSITIONING
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


# Everything a misbehaving network or device can cause; the follow loop must survive it.
_ERRORS = (OSError, ValueError, http.client.HTTPException, RaumfeldError, ET.ParseError)


# What one inspection of the room found; control/status are None unless the room is active in a zone.
_Room = namedtuple("_Room", "api zone power control status")


class Controller:
    """Drives the Raumfeld room. Taking over rediscovers everything via SSDP, because
    host IPs change and the zone renderer comes and goes; the frequent `check` reuses
    the last discovery until it fails or the zone changes."""

    def __init__(self, config, discover, api_port=RAUMFELD_API_PORT, sleep=time.sleep,
                 zone_wait_s=10.0, zone_poll_s=0.5, clock=time.monotonic):
        self.config = config
        self.discover = discover
        self.api_port = api_port
        self.sleep = sleep
        self.clock = clock
        self.zone_polls = int(zone_wait_s / zone_poll_s)
        self.zone_poll_s = zone_poll_s
        self._api = None
        self._control = None  # (zone UDN, AVTransport control URL)
        # The URI the zone held when the last takeover (not a re-takeover) set up our
        # stream; "" if none (standby, no zone, nothing loaded).
        self._source_before = ""

    def ensure_playing(self, wake=True, retake=False):
        """Make the room play our stream; returns the decision taken. With wake=False
        (a track change on a zone we already own) a room someone took away is left
        alone and its verdict returned instead. With retake=True (a takeover done again
        because the renderer did not start) a zone that plays another source than the one
        before the last takeover is left alone and OTHER_SOURCE returned: someone chose
        it since. The source before the takeover is taken over again: the speaker
        dropped our takeover."""
        room = self._inspect()
        if not wake:
            verdict = judge_speaker(room.power, room.zone, room.status, self.config.stream_url)
            if verdict in TAKEN_AWAY:
                return verdict
        if (retake and plays_another_source(room.status, self.config.stream_url)
                and not self._was_source_before(room.status.current_uri)):
            log.info("zone %r plays another source now; not taking it over again", room.zone)
            return OTHER_SOURCE
        if not retake:
            # Also when our stream already plays: an older source before is history then.
            uri = room.status.current_uri if room.status else ""
            self._source_before = "" if uri == self.config.stream_url else uri
        decision = decide_play(room.status, self.config.stream_url)
        if decision == ALREADY_PLAYING:
            log.debug("zone %r already plays the stream", room.zone)
            return decision
        if room.status and room.status.current_uri not in ("", self.config.stream_url):
            source = "line-in" if is_line_in(room.status.current_uri) else "another source"
            log.info("zone %r was on %s; the cast takes over", room.zone, source)

        room_param = quote(self.config.room_udn, safe="")
        zone, control = room.zone, room.control
        if wake:
            http_get(room.api + "leaveStandby?roomUDN=" + room_param)
            if room.power != "ACTIVE":
                # What was seen before the wake may belong to a speaker still shutting down.
                zone, control = self._wait_awake(room.api, had_zone=zone is not None), None
        zone = zone or self._connect_zone(room.api, room_param)
        control = control or self._zone_control(zone, reuse=False)
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

    def renderer_state(self):
        """Transport state of the zone renderer of the last takeover or look, e.g. PLAYING
        or STOPPED, with NOTHING_LOADED, PREVIOUS_SOURCE or ANOTHER_SOURCE appended unless
        it holds our stream. Only the renderer is asked: right after a wake the room's power
        state in getZones may still lag behind."""
        control = self._known_control()
        state = soap_call(control, "GetTransportInfo", []).get("CurrentTransportState", "")
        state = state[:MAX_STATE_LEN]  # device-supplied, and it ends up in log lines
        uri = soap_call(control, "GetMediaInfo", []).get("CurrentURI", "")
        if uri == self.config.stream_url:
            return state
        if not uri:
            return state + NOTHING_LOADED
        return state + (PREVIOUS_SOURCE if self._was_source_before(uri) else ANOTHER_SOURCE)

    def _was_source_before(self, uri):
        return bool(self._source_before) and uri == self._source_before

    def restart_renderer(self):
        """Stop, then Play, on the zone renderer of the last look at the room. A renderer that
        hangs in TRANSITIONING ignores a plain Play; Stop followed right away by Play brings
        it back. The stream URI is left as it is."""
        control = self._known_control()
        soap_call(control, "Stop", [])
        soap_call(control, "Play", [("Speed", "1")])
        log.info("zone %r restarted (Stop, Play)", self._control[0])

    def _known_control(self):
        if self._control is None:
            raise RaumfeldError("no zone renderer known yet")
        return self._control[1]

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

    def _wait_awake(self, api, had_zone):
        """The room's zone once getZones reports the room ACTIVE (and, if it had one before
        the wake, in a zone); after WAKE_WAIT_S whatever the last look that answered
        reported. Each look may only take what is left of the wait; one that fails or times
        out counts as "not awake yet". Raises the last error if no look answered."""
        deadline = self.clock() + WAKE_WAIT_S
        answered, error = False, None
        while True:
            try:
                zones = http_get(api + "getZones", deadline - self.clock())
                power = room_power_state(zones, self.config.room_udn)
                look_zone = zone_of_room(zones, self.config.room_udn)
            except _ERRORS as e:
                log.debug("room not awake yet: getZones failed: %r", e)
                error = e
            else:
                answered, zone = True, look_zone
                if power == "ACTIVE" and (zone or not had_zone):
                    return zone
            remaining = deadline - self.clock()
            if remaining <= 0:
                break
            self.sleep(min(WAKE_POLL_S, remaining))
            if self.clock() >= deadline:
                break
        if not answered:
            raise error
        log.warning("room not awake %g s after leaveStandby; taking over anyway", WAKE_WAIT_S)
        return zone

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


MpdStatus = namedtuple("MpdStatus", "state songid")  # songid is None without a current song

# A lost connection names MPD's connection_timeout (60 s by default) only after at least
# this much silence outside idle; after less it is far more likely an MPD restart or the
# network.
TIMEOUT_HINT_S = 30


class MpdClient:
    """Minimal MPD protocol client: `status`, `idle player`, `pause 1`/`pause 0` and
    `sendmessage`. Never `stop`: the receiver takes an MPD stop for the end of the track
    and advances."""

    def __init__(self, host, port, timeout=HTTP_TIMEOUT_S):
        self.timeout = timeout
        self.buf = b""
        # MPD hangs up on a client that sends nothing for its connection_timeout (60 s by
        # default) while not in `idle`, e.g. while the adapter waits for a slow speaker.
        # Tracked so a lost connection can say so.
        self._quiet_since = time.monotonic()
        self._silent_s = 0.0  # time outside idle before the last command
        self._idle_since = None  # set while in `idle`
        # A reset right after the connect shows at whichever step comes first; each is
        # reported as the lost connection it is.
        try:
            self.sock = socket.create_connection((host, port), timeout=timeout)
        except ConnectionResetError as e:
            raise self._lost() from e
        try:
            try:
                self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
            except OSError as e:
                if e.errno != errno.EINVAL:  # macOS: EINVAL on a socket already reset
                    raise
                raise self._lost() from e
            banner = self._readline()
            if not banner.startswith("OK MPD "):
                raise MpdError("not an MPD server: %r" % banner[:40])
        except BaseException:
            self.close()
            raise

    def close(self):
        self.sock.close()

    def status(self):
        self._send("status")
        fields = dict(line.partition(": ")[::2] for line in self._response())
        if "state" not in fields:
            raise MpdError("status without state")
        return MpdStatus(fields["state"], fields.get("songid"))

    def pause(self):
        self._command("pause 1")

    def resume(self):
        self._command("pause 0")

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
        self._idle_since = time.monotonic()
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
            # MPD restarts its connection_timeout when an `idle` ends.
            self._idle_since = None
            self._quiet_since = time.monotonic()

    def _command(self, command):
        self._send(command)
        self._response()

    def _send(self, command):
        if self._idle_since is None:  # `noidle` ends a wait in idle, not a silence
            now = time.monotonic()
            self._silent_s, self._quiet_since = now - self._quiet_since, now
        try:
            self.sock.sendall(command.encode() + b"\n")
        except (BrokenPipeError, ConnectionResetError) as e:
            raise self._lost() from e

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
            try:
                chunk = self.sock.recv(4096)
            except ConnectionResetError as e:
                raise self._lost() from e
            if not chunk:
                raise self._lost()
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")

    def _lost(self):
        """The error for a connection MPD closed or reset (an empty read, a reset, a broken
        pipe: which one shows depends on timing), naming connection_timeout only when the
        adapter had been silent outside idle for at least TIMEOUT_HINT_S and for longer than
        it has been in idle since: a drop late in a long idle is an MPD restart or the
        network, not the timeout."""
        idle_s = 0.0 if self._idle_since is None else time.monotonic() - self._idle_since
        if self._silent_s >= TIMEOUT_HINT_S and self._silent_s > idle_s:
            return ConnectionError(
                "MPD closed the connection after %.1f s without a command from the adapter "
                "(MPD hangs up on clients silent outside idle for longer than its "
                "connection_timeout, 60 s by default)" % self._silent_s)
        if self._idle_since is None:
            return ConnectionError("MPD closed the connection")
        return ConnectionError("MPD closed the connection while the adapter waited in idle "
                               "for %.1f s" % idle_s)


# --------------------------------------------------------------------------- orchestration

# Delay before re-trying a failed takeover while MPD keeps playing.
PLAY_RETRY_S = 30
# Shortest idle wait: a zero timeout would turn the MPD socket non-blocking.
MIN_WAIT_S = 0.1
DEFAULT_POLL_S = 5
# Consecutive polls that may find our renderer stopped while MPD plays (a stream
# reconnect between songs) before the speaker counts as taken away.
STOPPED_GRACE_POLLS = 2
# A renderer that stays TRANSITIONING on our stream this long while MPD plays is stuck
# (it dropped the stream). Restarts per stuck episode, each waiting twice as long as the
# one before; a renderer seen PLAYING again ends the episode.
STUCK_S = 15
STUCK_MAX_RESTARTS = 3
# Start gate: at a takeover MPD is held at the start of the song (pause 1) until the
# renderer plays our stream, at most START_GATE_S seconds; the renderer is asked every
# GATE_POLL_S seconds meanwhile.
START_GATE_S = 15
GATE_POLL_S = 0.5
# Highest START_GATE_SECONDS, and the hard bound on the hold however the gate is extended:
# holding the song any longer only leaves the phone standing still.
START_GATE_MAX_S = 60
# A speaker still finishing its standby when woken accepts the takeover and drops it. So a
# renderer that has not loaded or does not play our stream (stopped, no media, a URI not
# ours, no answer) GATE_RETAKE_S after the takeover returned gets the whole takeover again,
# at most GATE_MAX_RETAKES times per gate, and only while it can still have its wake wait
# and GATE_RETAKE_S before START_GATE_MAX_S. Every takeover in the gate may push its end to
# GATE_RETAKE_S after it returned (never past START_GATE_MAX_S), so it has time to play.
# Never taken over again: a renderer playing another source than the one before the
# takeover (someone chose it: the gate yields to it, MPD stays paused) or one
# TRANSITIONING on our stream (buffering); the latter is restarted (Stop, Play) once if
# still TRANSITIONING 2 * GATE_RETAKE_S after the takeover, and after the gate the stuck
# handling (STUCK_S) takes over.
GATE_RETAKE_S = 5
GATE_MAX_RETAKES = 2

# What the Follower asks run() to do on the MPD connection.
PAUSE_PLAYER = "pause"
RESUME_PLAYER = "resume"
RELEASE_CAST = "release_cast"

_PLAY = "ensure_playing"
_POLL = "check"
_GATE = "gate"

# Start gate phases: our pause 1 is on its way; MPD is held while the speaker wakes and
# buffers; our pause 0 is on its way.
_PAUSING = "pausing"
_HOLDING = "holding"
_RESUMING = "resuming"


class _Gate:
    def __init__(self, song, started, seconds):
        self.phase = _PAUSING
        self.song = song
        self.started = started
        self.deadline = started + seconds
        self.taken = None  # when the last takeover (or restart) in the gate returned
        self.retakes = 0
        self.restarted = False  # a renderer TRANSITIONING on our stream was restarted
        self.seen = "not asked yet"  # the last renderer state, for the timeout warning

    def fits(self, now, wake_s):
        """A takeover starting now, after a wake wait of wake_s, still has GATE_RETAKE_S
        to play before the gate's hard bound."""
        return now + wake_s + GATE_RETAKE_S <= self.started + START_GATE_MAX_S

    def retake_due(self, now):
        return (self.seen != "TRANSITIONING" and self.retakes < GATE_MAX_RETAKES
                and now >= self.taken + GATE_RETAKE_S and self.fits(now, WAKE_WAIT_S))

    def restart_due(self, now):
        return (self.seen == "TRANSITIONING" and not self.restarted
                and now >= self.taken + 2 * GATE_RETAKE_S and self.fits(now, 0))


# What renderer_state reports for a renderer someone made play another source than the one
# before the takeover: never taken over again. (Back on the source before the takeover, the
# speaker dropped our takeover: that is taken over again.)
_OTHER_SOURCE_PLAYING = tuple(state + ANOTHER_SOURCE for state in _PLAYING_STATES)

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
    end: it just lets go (no pause, no release) and the next play wakes the room.

    Start gate: when MPD starts playing and the adapter doesn't own the zone, it first
    asks run() to pause MPD, takes over only once the pause is confirmed, and asks for
    pause 0 as soon as the renderer plays our stream (or after start_gate_seconds). A
    renderer that does not start gets the takeover again (GATE_RETAKE_S,
    GATE_MAX_RETAKES), one TRANSITIONING on our stream a restart instead; if it still has
    never played when the gate ends, it gets one more takeover before it counts as taken
    away. A renderer someone made play another source than the one before the takeover
    ends the gate at once: it yields (MPD stays paused, the cast is released). Its own pause and resume are not followed; a failed
    takeover lets MPD play at once and is retried after PLAY_RETRY_S without the gate. A
    stop or another song during the gate is the user's doing and is never undone; a pause
    from the phone during the gate can't be told apart from ours (MPD is already paused,
    nothing changes), so it is undone."""

    def __init__(self, controller, poll_seconds=DEFAULT_POLL_S, clock=time.monotonic,
                 stuck_seconds=STUCK_S, start_gate_seconds=START_GATE_S):
        self.controller = controller
        self.poll_seconds = poll_seconds
        self.stuck_seconds = stuck_seconds
        self.start_gate_seconds = start_gate_seconds
        self.clock = clock
        self._gate = None  # the start gate in progress, if any
        self._stuck = None  # (first seen, restarts so far, next restart due) of a stuck episode
        # A start gate timed out and every look at the room since found our renderer
        # stopped: it never started, so nobody can have stopped it; one more takeover is
        # due before the speaker counts as taken away.
        self._unstarted = False
        self.pending = None
        self.deadline = None
        self.mpd_state = None
        self.mpd_song = None
        self.owned = False
        self.yielded = False
        self.wake = True
        self.stopped_polls = 0
        self._requests = []

    def on_mpd_connected(self):
        """A new MPD connection: its first state is followed even if it looks unchanged.
        An MPD that restarted and restored play has a new stream (the renderer lost the old
        one) and song ids that start again at 0, so the same songid is no proof of the same
        playback."""
        self.mpd_song = None

    def on_mpd_state(self, state, songid=None):
        if songid is not None and (state, songid) == (self.mpd_state, self.mpd_song):
            # No change within this connection: the idle event of our own pause 0 after the
            # status read that already reported it, or a seek. Taken as a new start it would
            # hold MPD again at once.
            return
        previous, self.mpd_state, self.mpd_song = self.mpd_state, state, songid
        self.stopped_polls = 0
        if self._gate is not None:
            if self._gated(state, songid):
                return
            # The gate was ended by someone else; our own pause was no user pause.
            previous = "play"
        # A user change: a renderer stopped from now on is no longer ours to restart.
        self._unstarted = False
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
        if state == "play" and not self.owned and self.start_gate_seconds > 0:
            self._start_gate(songid)
        elif state == "play":
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

    def _start_gate(self, song):
        log.info("holding MPD at the start of the song while the speaker wakes (at most %g s)",
                 self.start_gate_seconds)
        self._gate = _Gate(song, self.clock(), self.start_gate_seconds)
        self._requests.append(PAUSE_PLAYER)
        # Should our pause never be confirmed, the gate deadline still takes over.
        self.pending, self.deadline = _GATE, self._gate.deadline

    def _gated(self, state, song):
        """Handle an MPD state during the gate; False once someone else ended it."""
        gate = self._gate
        if gate.phase == _PAUSING and state == "pause":
            # Our pause 1 holds the song: now wake the room.
            gate.phase, gate.song = _HOLDING, song
            self._gate_take_over()
            return True
        if gate.phase == _PAUSING and state == "play":
            # Our pause 1 is still to go out (requeued at a reconnect) and holds whatever
            # song MPD plays by then.
            return True
        if state == "pause" and song == gate.song and gate.phase != _PAUSING:
            return True  # ours (an event that arrives late), or a phone pause we can't tell apart
        if gate.phase == _RESUMING and state == "play" and song == gate.song:
            self._gate = None  # our pause 0 landed
            return True
        log.info("MPD changed during the start gate (%r, song %r); leaving it as it is",
                 state, song)
        self._gate = None
        self._requests = [r for r in self._requests if r not in (PAUSE_PLAYER, RESUME_PLAYER)]
        if self.pending == _GATE:
            self.pending = None
        return False

    def _gate_take_over(self):
        self.wake = True
        self._attempt(_PLAY)
        if self.owned:
            self._give_time()
        else:
            # The takeover failed and is retried later; never keep the phone waiting for it.
            self._release_gate("the takeover failed")

    def _gate_check(self):
        if self._gate.phase == _PAUSING:
            # Our pause 1 never landed: play ungated.
            log.warning("MPD was not paused within %g s; taking over without the start gate",
                        self.start_gate_seconds)
            self._gate = None
            self._requests = [r for r in self._requests if r != PAUSE_PLAYER]
            self.wake = True
            return self._attempt(_PLAY)
        gate = self._gate
        try:
            gate.seen = self.controller.renderer_state()
        except _ERRORS as e:
            log.debug("start gate: renderer check failed: %r", e)
            gate.seen = "check failed: %r" % e
        now = self.clock()
        if gate.seen == "PLAYING":  # our stream: renderer_state flags any other source
            log.info("speaker plays; resuming MPD after %.1f s", now - gate.started)
            self._end_gate()
        elif gate.seen in _OTHER_SOURCE_PLAYING:
            # Someone chose another source during the gate: it is not ours to override.
            self._gate_yield("the zone plays another source (renderer %r)" % gate.seen)
        elif gate.restart_due(now):
            self._gate_restart(now)
        elif gate.retake_due(now):
            self._gate_retake(now)
        elif now >= gate.deadline:
            self._gate_timeout(now)
        else:
            self._schedule(_GATE)

    def _gate_retake(self, now):
        gate = self._gate
        gate.retakes += 1
        log.info("start gate: renderer %r %.1f s after the takeover; taking over again "
                 "(%d of %d)", gate.seen, now - gate.taken, gate.retakes, GATE_MAX_RETAKES)
        try:
            decision = self.controller.ensure_playing(wake=True, retake=True)
        finally:
            # A failed retake counts and gets its time too; its error propagates up to the
            # _attempt that runs this gate check, which logs it.
            self._give_time()
        if decision in TAKEN_AWAY:
            self._gate_yield("someone started another source during the takeover")

    def _gate_restart(self, now):
        gate = self._gate
        gate.restarted = True
        log.info("start gate: renderer still TRANSITIONING %.1f s after the takeover; "
                 "restarting it (Stop, Play)", now - gate.taken)
        try:
            self.controller.restart_renderer()
        finally:
            self._give_time()

    def _give_time(self):
        # Every takeover or restart gets GATE_RETAKE_S to start playing, even past the
        # configured gate.
        gate = self._gate
        gate.taken = self.clock()
        gate.deadline = min(max(gate.deadline, gate.taken + GATE_RETAKE_S),
                            gate.started + START_GATE_MAX_S)
        self._schedule(_GATE)

    def _gate_timeout(self, now):
        try:
            room = self.controller.check()
        except _ERRORS as e:
            room = "check failed: %r" % e
        self._unstarted = True
        self._release_gate("the renderer does not play after %.1f s (renderer %r, room %r)"
                           % (now - self._gate.started, self._gate.seen, room))

    def _gate_yield(self, reason):
        """End the gate by yielding: MPD stays paused (ours, so no new pause), the cast is
        released and the room left alone."""
        log.info("start gate: %s; not taking it over", reason)
        self._gate = None
        self._requests = [r for r in self._requests if r not in (PAUSE_PLAYER, RESUME_PLAYER)]
        self.pending = None
        self._yield(OTHER_SOURCE)

    def _release_gate(self, reason):
        log.warning("start gate: %s; resuming MPD anyway", reason)
        self._end_gate()

    def _end_gate(self):
        self._gate.phase = _RESUMING
        self._requests.append(RESUME_PLAYER)
        if self.pending in (None, _GATE):
            self._schedule(_POLL)

    def _attempt(self, action):
        self.pending = None
        try:
            if action == _PLAY:
                self._take_over()
            elif action == _GATE:
                self._gate_check()
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
        if verdict != NOT_PLAYING:
            self._unstarted = False
        if verdict == NOT_PLAYING and self.mpd_state == "play":
            self.stopped_polls += 1
            if self.stopped_polls > STOPPED_GRACE_POLLS and self._unstarted:
                return self._take_over_unstarted()
            if self.stopped_polls > STOPPED_GRACE_POLLS:
                return self._yield(verdict)
        elif verdict in (OURS, TRANSITIONING):
            self.stopped_polls = 0
        stuck = verdict == TRANSITIONING and self.mpd_state == "play"
        if not stuck and verdict != NOT_PLAYING:
            # Our own Stop may read NOT_PLAYING; it must not end the episode it belongs to.
            self._stuck = None
        # Schedule first: a failing restart is logged by _attempt and must not stop polling.
        self._schedule(_POLL)
        if stuck:
            self._unstick()

    def _unstick(self):
        now = self.clock()
        if self._stuck is None:
            self._stuck = (now, 0, now + self.stuck_seconds)
            return
        since, restarts, due = self._stuck
        if now < due or restarts > STUCK_MAX_RESTARTS:
            return
        if restarts == STUCK_MAX_RESTARTS:
            # restarts > STUCK_MAX_RESTARTS marks "gave up", so this is logged once.
            log.error("zone renderer still TRANSITIONING after %d restarts; giving up until "
                      "it plays again", restarts)
            self._stuck = (since, restarts + 1, due)
            return
        restarts += 1
        self._stuck = (since, restarts, now + self.stuck_seconds * 2 ** restarts)
        log.warning("zone renderer TRANSITIONING for %.0f s while MPD plays: it dropped the "
                    "stream; restarting it (Stop, Play), attempt %d of %d",
                    now - since, restarts, STUCK_MAX_RESTARTS)
        self.stopped_polls = 0
        self.controller.restart_renderer()

    def _take_over_unstarted(self):
        log.info("zone renderer has never played since the start gate gave up; taking over "
                 "once more before the speaker counts as taken away")
        self._unstarted = False
        self.stopped_polls = 0
        # Schedule first: a failing takeover propagates up to the _attempt that runs this
        # poll, which logs it; polling must go on.
        self._schedule(_POLL)
        decision = self.controller.ensure_playing(wake=True, retake=True)
        if decision in TAKEN_AWAY:
            self.pending = None
            self._yield(decision)

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
        if action == _GATE:
            self.deadline = min(self.clock() + GATE_POLL_S, self._gate.deadline)
        else:
            self.deadline = self.clock() + (PLAY_RETRY_S if action == _PLAY else self.poll_seconds)


def _serve_requests(follower, mpd):
    # Reporting a pause or resume back may queue more (the start gate takes over right
    # after its pause 1 and may ask for pause 0 at once), so serve until none are left.
    while True:
        requests = follower.take_mpd_requests()
        if not requests:
            return
        for i, request in enumerate(requests):
            try:
                if request in (PAUSE_PLAYER, RESUME_PLAYER):
                    if request == PAUSE_PLAYER:
                        mpd.pause()
                    else:
                        mpd.resume()
                    # Note the change before the message goes out: a receiver that ends the
                    # session right away would otherwise fold our pause and its stop into
                    # one MPD event.
                    follower.on_mpd_state(*mpd.status())
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
            follower.on_mpd_connected()
            follower.on_mpd_state(*mpd.status())
            # Tick right away too: a deadline may have passed while MPD was unreachable.
            follower.tick()
            _serve_requests(follower, mpd)
            while not should_stop():
                wait = follower.seconds_until_deadline()
                # Only a real player change is an MPD event; a timeout just means "tick".
                if mpd.wait_player(None if wait is None else max(wait, MIN_WAIT_S)):
                    follower.on_mpd_state(*mpd.status())
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
    follower = Follower(controller, config.poll_seconds, stuck_seconds=config.stuck_seconds,
                        start_gate_seconds=config.start_gate_seconds)
    try:
        run(follower, lambda: MpdClient(config.mpd_host, config.mpd_port))
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
