# Raumfeld adapter

A small sidecar (Python 3.9+, standard library only) that makes a Teufel
Raumfeld room play the receiver's MPD stream automatically.

- When MPD starts playing, it wakes the room (`leaveStandby`), reuses the room's existing zone (or connects
  it to a new one only if it has none) and tells the zone's renderer to play
  `STREAM_URL` via UPnP AVTransport. **Waking waits for the room:** if the room
  was not `ACTIVE`, the adapter asks `getZones` every 0.5 s until it reports the
  room `ACTIVE` (and in a zone, if it had one before), at most 10 s in all,
  slow answers included (`WAKE_WAIT_S`), and only then sends
  `SetAVTransportURI`/`Play` to the renderer of the zone it is in now. A speaker woken while it is still
  finishing its standby otherwise accepts those requests and drops them. A look
  that fails or times out counts as "not awake yet". A room not awake in time is
  taken over anyway, in the zone the last answer named, with a warning; the
  start gate's re-takeover (below) covers the rest. Only if no look answered at
  all does the takeover fail (and is retried, see below).
- **Finding the devices** (by UDN, never trusting an address alone): the
  Raumfeld host that serves the API (`getZones`, `leaveStandby`, port 47365) and
  the zone renderer (UPnP AVTransport). They are looked for once at adapter
  start, so the first cast finds them cached, and again only when a request to
  the cached address fails (then the request is sent once more to the address
  found afresh) or the room is in another zone (its renderer is looked for by the
  new zone UDN). With a valid cache a takeover sends no SSDP at all.
  - SSDP searches are multicast (`239.255.255.250:1900`) with `MX: 1`, re-sent
    each second, and return at the first matching answer (the Raumfeld answers
    in about 0.3 s with `MX: 1`, about 2 s with `MX: 2`).
  - Without `RAUMFELD_HOST`: the host by a search for `RAUMFELD_SPEAKER_UDN`, the
    zone renderer by one for the zone UDN (each up to 6 s if nobody answers);
    any answering device counts, and its description is checked.
  - With `RAUMFELD_HOST` (an IPv4 address or a DNS name): the API is reached
    at `http://<host>:47365/` directly; a name is resolved once (IPv4) and again
    after a failed request. The zone renderer is found by a search for the zone
    UDN (`ST:` the zone UDN) of which only answers sent from the host's address
    count; others are ignored (at most 3 s). The Raumfeld does **not** answer a
    unicast M-SEARCH sent to `<host>:1900`, so the setting filters multicast
    answers instead of replacing multicast.
  - Either way: a host whose `getZones` doesn't list `RAUMFELD_ROOM_UDN` is an
    error (a wrong host), and a renderer description whose `UDN` is not the zone
    UDN is refused: never a wrong device. If nothing is found at start, that is
    logged and the first cast looks again.
  - The start gate's renderer looks (every 0.25 s) use the cached renderer and
    don't look for it again when one fails; the next takeover or room check does.
- **Start gate:** the speaker needs several seconds to wake, join its zone and
  buffer, while MPD starts the song right away. So when MPD starts playing and
  the adapter doesn't own the room (a new cast, or a resume after the room was
  taken away), it first sends MPD `pause 1`, which holds the song at about 0:00;
  MPD's httpd output keeps streaming silence while paused (`always_on`), so the
  speaker can connect. Then it wakes the room and starts the stream as usual,
  asks the zone renderer every 0.25 s whether it is `PLAYING` on `STREAM_URL`,
  and sends `pause 0` as soon as it is. **Re-takeover:** if the renderer has not
  loaded or does not play `STREAM_URL` 5 s (`GATE_RETAKE_S`) after the takeover
  returned (`STOPPED`, `NO_MEDIA_PRESENT`, no URI at all whatever its state, a
  URI not ours that does not play, back on the source it played before the
  takeover, e.g. the line-in the cast took over, the room back in standby or out
  of its zone, or no answer), the adapter does the whole takeover again (wake,
  wait for the room, `SetAVTransportURI`, `Play`; same UDN discovery, no
  volume), at most twice per gate (`GATE_MAX_RETAKES`), and logs each one at
  INFO with the state that triggered it. A re-takeover only starts if it can
  still have its wake wait and its 5 s before the 60 s mark. Never taken over
  again: a renderer `TRANSITIONING` on the stream (buffering): if it is still
  `TRANSITIONING` 10 s after the takeover returned, it is restarted once
  (`Stop`, `Play`, as for a stuck renderer, below); the gate lasts that long for
  it even after a slow takeover (a speaker woken from standby), and the restart
  gets its own 5 s, never past 60 s. If it still is `TRANSITIONING` when the
  gate ends, the stuck-renderer handling carries on from there. Nor a renderer
  that plays a source other than the one before the takeover (someone chose it
  during the gate, also if a re-takeover finds it; after a takeover from standby
  any source is such a choice): the gate ends at once by yielding, MPD stays
  paused (no `pause 0`) and the adapter sends the release message. Every
  takeover or restart in the gate gets those 5 s to start playing, so the gate
  may run past `START_GATE_SECONDS` (by default up to about 25 s with two
  re-takeovers on a speaker answering in 3 s; more on a slower one), but never
  past 60 s, the highest `START_GATE_SECONDS`. If the renderer still doesn't
  play by then, or the takeover fails, it sends `pause 0` anyway and logs a
  warning naming the last renderer state and the room verdict (e.g. `renderer
  'STOPPED', room 'not_playing'` or `room 'standby'`): playback is never
  blocked. A failed takeover is retried after 30 seconds without holding MPD
  again; MPD's report of the adapter's own `pause 0` (same state, same `songid`)
  is no new start. That rule applies within one MPD connection: after a
  reconnect the first state is followed afresh (a restarted MPD reuses song
  ids). The adapter's own pause is not followed to the speaker (no AVTransport
  `Pause`, no "taken away"). **Pre-roll:** the receiver starts MPD on endless
  silence at the DIAL launch (see the main README), so the gate normally starts
  on the silence and the song replaces it later, MPD going from playing the
  silence straight to playing the song (a new `songid`, no stop). If that
  happens during the gate, the gate carries on with the song: the adapter sends
  `pause 1` again if MPD went to play, so the song too is held at about 0:00
  until the speaker plays, with the gate's timing and re-takeover count
  unchanged and without a new takeover. A skip from the phone during the gate
  is held the same way (holding at the start of the new song undoes nothing).
  If the song arrives while the adapter's `pause 0` is on its way, the speaker
  already plays: the gate ends and MPD plays the song. If the song arrives after
  the gate (the speaker already plays the silence), there is no gate and no
  takeover: the stream just goes on with the song. If MPD stops during the gate
  (the phone, or the receiver ending the silence after 60 s), the adapter does
  not unpause. **Limit:** a pause from the phone during the gate can't be
  told apart from the adapter's own pause (MPD is already paused, so nothing
  changes), and is undone when the gate opens. Song changes and resumes while
  the adapter owns the room are not gated: the speaker already plays the stream.
  The phone shows the position standing still during the gate.
- If the zone already plays the stream, it does nothing. **An explicit cast
  wins at takeover:** a new cast or a user resume takes the room over from any
  other source, e.g. the Raumfeld **line-in** (a Chromecast Audio or a karaoke
  machine) or another stream, even if it reports `PLAYING`. The Raumfeld stays
  on line-in after Home Assistant switches it there, and line-in reports
  `PLAYING` without any audio, so "playing" can't tell a busy speaker from an
  idle one.
- **The speaker's own controls win.** While the adapter owns the zone (MPD
  playing, paused or stopped), it looks at the room every `SPEAKER_POLL_SECONDS` (one
  `getZones` plus `GetTransportInfo`/`GetMediaInfo` at the cached addresses,
  see above). The speaker counts as
  *taken away* when the room is no longer `ACTIVE` (power button, or the
  Raumfeld's own auto power-off, also while paused), the room left its zone,
  the zone plays anything other than `STREAM_URL` (e.g. line-in switched in by
  Home Assistant), or, while MPD plays, the renderer stays stopped for more than
  two polls in a row. **Exception right after a start gate that ran out:** if
  every look at the room since then found the renderer stopped, it never started
  after the adapter's own takeover, so nobody stopped it; instead of yielding, the
  adapter does one more takeover and logs it, then counts again (should another
  source play by then, the speaker counts as taken away). A renderer seen
  playing or `TRANSITIONING` since, or any change in MPD (pause, stop, another
  song), drops that exception, so a user who stops the speaker later is still
  respected. If the speaker counts as taken away while MPD is playing or paused,
  the adapter:
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
- **A stuck renderer is restarted.** If, while MPD plays, the zone renderer
  reports `TRANSITIONING` on `STREAM_URL` for longer than `STUCK_SECONDS`
  (measured across polls), it has dropped the stream and hangs (seen after a
  speaker request timed out at a song change). A plain AVTransport `Play` does not
  help, so the adapter sends `Stop` and right away `Play` and logs a warning. This
  is not "taken away": no pause, no release. At most 3 restarts per stuck
  episode, each waiting twice as long as the one before; then it logs an error and
  leaves the renderer alone. A renderer seen `PLAYING` again ends the episode. A
  short `TRANSITIONING` (normal start or buffering) is left alone.
- How the timings fit together: one takeover may wait up to 10 s for the room
  to wake (plus the request time, and SSDP if a device has to be looked for); a re-takeover is due 5 s after
  a takeover returned; the gate ends at `START_GATE_SECONDS` or 5 s after the
  last takeover returned, whichever is later, at most 60 s after it began. A
  re-takeover only starts at least 15 s (wake wait plus its 5 s) before that
  60 s mark, a restart at least 5 s before it. So while the renderer
  doesn't play, a gate set below about 15 s still lasts until both re-takeovers
  had their 5 s; a renderer that plays ends the gate at once, whatever the
  setting.
- Apart from these restarts it never stops the zone, and it never puts the room
  into standby itself; the Raumfeld's own auto power-off does that.
- It never changes the Raumfeld's volume and sends no RenderingControl requests
  at all. Volume from the phone is MPD's software volume: the httpd output needs
  `mixer_type "software"` (as in [`mpd.conf.example`](mpd.conf.example)),
  otherwise MPD answers the phone's volume change with `ACK ... {setvol} No mixer`.
- Devices are untrusted: it only fetches plain `http://` URLs on the host that
  answered SSDP or is `RAUMFELD_HOST` (redirects included), caps replies at 256 KiB and each request
  at 10 s in total (connect, headers, body), refuses XML with DOCTYPE/ENTITY,
  and accepts only `uuid:<letters, digits, dashes>` UDNs.
- Network and device errors are logged, never fatal, and never count as "taken
  away". If the MPD connection drops while `pause 1`, `pause 0` or the release
  message is being sent, whatever is left is sent again after the reconnect. A
  failed takeover is retried on the next change in MPD's state or after 30
  seconds while MPD keeps playing; a failed poll is retried at the next poll.
  Lost MPD connections are re-established with backoff.
- Speaker requests run between MPD commands, outside MPD's `idle`. MPD hangs up on
  a client that stays silent there for longer than its `connection_timeout` (60 s
  by default), so a speaker that keeps answering slowly (each request up to its
  10 s deadline, plus SSDP) can cost the MPD connection. The adapter then
  reconnects and re-reads MPD's state; the warning says how long it had sent MPD
  nothing, so such a drop can be told apart from MPD going away. It names
  `connection_timeout` only when the adapter had been silent outside `idle` for at
  least 30 s (whether the drop shows as a closed connection, a reset or a broken
  pipe); a drop while it waited in `idle`, e.g. an MPD restart, is reported as such.

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
| `RAUMFELD_SPEAKER_UDN` | required without `RAUMFELD_HOST` | UDN of the speaker device that hosts the Raumfeld API; found by multicast SSDP. Not used with `RAUMFELD_HOST` (checked if set) |
| `RAUMFELD_HOST` | unset | IPv4 address or DNS name of the Raumfeld host (no port, scheme or path), e.g. `<raumfeld-host>.local`. Reaches its API directly (no SSDP for the host) and takes only that address's answer to the multicast search for the zone renderer (the Raumfeld doesn't answer unicast SSDP). The room and zone UDNs are still checked: a wrong host is an error |
| `STREAM_URL` | required | MPD httpd stream (`http://` or `https://`), e.g. `http://<receiver-host>:8000/stream.mp3` |
| `STREAM_MIME` | `audio/mpeg` | MIME type announced in the DIDL-Lite metadata |
| `STREAM_METADATA` | `1` | `0` sends empty `CurrentURIMetaData` instead of DIDL-Lite |
| `SPEAKER_POLL_SECONDS` | `5` | How often (seconds, at least 1) the room is checked while the adapter owns the zone |
| `START_GATE_SECONDS` | `15` | How long (seconds, 0-60) MPD is held at the start of the song while the speaker wakes; `0` turns the start gate off. A takeover in the gate always gets 5 s to start playing, so the hold can run past this value, but never past 60 s: holding the song any longer only leaves the phone standing still |
| `STUCK_SECONDS` | `15` | How long (seconds, 1-300) the renderer may stay `TRANSITIONING` on the stream while MPD plays before it is restarted with `Stop`, `Play` |
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

SSDP needs multicast on the LAN (with or without `RAUMFELD_HOST`: the Raumfeld
doesn't answer unicast searches), so in a container use host networking.

## Pre-roll and cast start timing

The receiver plays endless silence from the DIAL launch until the song is ready
(see the main README). So a cast normally goes: MPD plays the silence, the start
gate holds it and wakes the room while the phone still loads the song; the song
then replaces the silence, during the gate (held at 0:00 until the speaker
plays) or after it (it plays at once, the speaker already plays the stream).
Either way there is only one takeover. If no song comes, the receiver stops the
silence after 60 s, which also ends a gate still running.

Each cast start is logged at INFO, one line per phase, times from the moment MPD
started playing (the gate's start), e.g.:

```
start: takeover returned at +2.6 s (wake wait 2.1 s, cached addresses, no discovery)
start: song '2' at +2.2 s, speaker not playing yet: held
start: speaker plays at +4.3 s; resuming MPD
start: MPD resumed at +4.3 s (song '2' held 2.1 s)
```

A song after the gate shows as `start: song '2' at +6.0 s, speaker already
playing: no hold`. `N discoveries` instead of `cached addresses` says the
takeover had to look for N devices (host, zone renderer) first.

## Tests

```sh
python3 -m unittest discover -s adapter -v
```

The tests use local fake servers (MPD, Raumfeld host API, device description,
AVTransport SOAP, SSDP responders standing in for the multicast group) on
127.0.0.1 and need no real devices; no multicast leaves the machine.
