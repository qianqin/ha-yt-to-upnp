# Raumfeld adapter

A small sidecar (Python 3.9+, standard library only) that makes a Teufel
Raumfeld room play the receiver's MPD stream automatically.

- When MPD starts playing, it finds the speaker via SSDP (by UDN, never by IP),
  wakes the room (`leaveStandby`), reuses the room's existing zone (or connects
  it to a new one only if it has none) and tells the zone's renderer to play
  `STREAM_URL` via UPnP AVTransport.
- If the zone already plays the stream, it does nothing. **An explicit cast
  wins at takeover:** a new cast or a user resume takes the room over from any
  other source, e.g. the Raumfeld **line-in** (a Chromecast Audio or a karaoke
  machine) or another stream, even if it reports `PLAYING`. The Raumfeld stays
  on line-in after Home Assistant switches it there, and line-in reports
  `PLAYING` without any audio, so "playing" can't tell a busy speaker from an
  idle one.
- **The speaker's own controls win.** While the adapter owns the zone (MPD
  playing, paused or stopped), it looks at the room every `SPEAKER_POLL_SECONDS` (one
  `getZones` plus `GetTransportInfo`/`GetMediaInfo`, reusing the last SSDP
  discovery until a request fails or the zone changes). The speaker counts as
  *taken away* when the room is no longer `ACTIVE` (power button, or the
  Raumfeld's own auto power-off, also while paused), the room left its zone,
  the zone plays anything other than `STREAM_URL` (e.g. line-in switched in by
  Home Assistant), or, while MPD plays, the renderer stays stopped for more than
  two polls in a row. If that happens while MPD is playing or paused, the
  adapter:
  1. sends MPD `pause 1` if MPD is playing (never `stop`: the receiver reads an
     MPD stop as the end of the track and plays the next one),
  2. sends `sendmessage ytcast release` once, which tells the receiver to end the
     cast session (see below),
  3. stops polling and leaves the room alone. A song change does not wake the
     room again; if one slips in, MPD is paused again. Only a user resume from
     the phone (MPD pause -> play) lets it take the room back, waking it as
     usual; a new session (MPD pause -> stop, then play) takes over as usual too,
     even if another source plays at that moment.
- If the room goes away while MPD is **stopped** (the cast session already
  ended, e.g. the Raumfeld's auto power-off later), there is nothing to end:
  the adapter just lets go of the room and stops polling, without pause or
  release, and the next cast wakes the room as usual.
- A song change on a zone the adapter owns never wakes the room: if the room
  was switched off or to another source in the meantime, that counts as taken
  away right away. Trade-off: if the room is switched off and a poll lands in
  the short MPD stop between two songs, the adapter has already let go, so the
  next song wakes the room again.
- It never stops the zone or puts the room into standby itself; the Raumfeld's
  own auto power-off does that.
- It never changes the Raumfeld's volume and sends no RenderingControl requests
  at all. Volume from the phone is MPD's software volume: the httpd output needs
  `mixer_type "software"` (as in [`mpd.conf.example`](mpd.conf.example)),
  otherwise MPD answers the phone's volume change with `ACK ... {setvol} No mixer`.
- Devices are untrusted: it only fetches plain `http://` URLs on the host that
  answered SSDP (redirects included), caps replies at 256 KiB and each request
  at 10 s in total (connect, headers, body), refuses XML with DOCTYPE/ENTITY,
  and accepts only `uuid:<letters, digits, dashes>` UDNs.
- Network and device errors are logged, never fatal, and never count as "taken
  away". If the MPD connection drops while `pause 1` or the release message is
  being sent, whatever is left is sent again after the reconnect. A failed takeover is retried on the next MPD event or after 30 seconds
  while MPD keeps playing; a failed poll is retried at the next poll. Lost MPD
  connections are re-established with backoff.

## Ending the cast: MPD channel `ytcast`

The adapter talks to the receiver through MPD's client-to-client messages: it
sends the message `release` on the channel `ytcast`
(`sendmessage ytcast release`). The receiver subscribes to that channel and
ends the phone's cast session when it gets `release`. With a receiver that
doesn't subscribe, MPD answers "nobody is subscribed to this channel"; the
adapter logs a warning and carries on, and the earlier `pause 1` still stops
the stream (the phone shows it paused).

## Configuration (environment)

| Variable | Default | Meaning |
| --- | --- | --- |
| `RAUMFELD_ROOM_UDN` | required | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001` |
| `RAUMFELD_SPEAKER_UDN` | required | UDN of the speaker device that hosts the Raumfeld API |
| `STREAM_URL` | required | MPD httpd stream (`http://` or `https://`), e.g. `http://<receiver-host>:8000/stream.mp3` |
| `STREAM_MIME` | `audio/mpeg` | MIME type announced in the DIDL-Lite metadata |
| `STREAM_METADATA` | `1` | `0` sends empty `CurrentURIMetaData` instead of DIDL-Lite |
| `SPEAKER_POLL_SECONDS` | `5` | How often (seconds, at least 1) the room is checked while the adapter owns the zone |
| `MPD_HOST` / `MPD_PORT` | `localhost` / `6600` | MPD control connection |
| `LOG_LEVEL` | `INFO` | `DEBUG`, `INFO`, `WARNING`, `ERROR`, `CRITICAL` |

Missing or invalid settings make the adapter exit with status 2 and a message
naming the variable. `IDLE_MINUTES` is no longer used; if it is set, the
adapter logs a warning and ignores it.

See [`mpd.conf.example`](mpd.conf.example) for a matching MPD setup (httpd
output on port 8000, MP3 or FLAC, software mixer for the phone's volume).

## Run

From the repository root:

```sh
RAUMFELD_ROOM_UDN=uuid:... RAUMFELD_SPEAKER_UDN=uuid:... \
STREAM_URL=http://<receiver-host>:8000/stream.mp3 \
python3 -m adapter
# or: python3 adapter/raumfeld_adapter.py
```

SSDP needs multicast on the LAN, so in a container use host networking.

## Tests

```sh
python3 -m unittest discover -s adapter -v
```

The tests use local fake servers (MPD, Raumfeld host API, device description,
AVTransport SOAP, SSDP responder) on 127.0.0.1 and need no real devices.
