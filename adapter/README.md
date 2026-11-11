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
  new zone UDN). For the zone renderer "fails" means no answer (refused, reset,
  timed out): an error it answers with, e.g. a UPnP fault (HTTP 500), comes from
  the right device and is raised as it is, the request not sent twice. With a
  valid cache a takeover sends no SSDP at all.
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
    count; others are ignored (at most 3 s), and a matching answer from another
    address is logged once per search as a warning naming both addresses. So
    `RAUMFELD_HOST` must resolve to the address the host answers SSDP from (on a
    host with several interfaces, the one on the speakers' LAN). The Raumfeld does
    **not** answer a unicast M-SEARCH sent to `<host>:1900`, so the setting filters
    multicast answers instead of replacing multicast. An IPv4 address gives the
    firmest binding: no DNS, so nothing can re-point it, and it costs no lookup.
  - **Kept across restarts** with `ADAPTER_STATE_DIR`: after a search the zone
    renderer's location is written to `raumfeld-adapter-state.json` there (zone
    UDN, host address, description URL; nothing secret), atomically (temporary
    file, rename) with mode 0600. At start it is tried first: if its description
    still names the zone and it is on an acceptable host (with `RAUMFELD_HOST` that
    host's address; without it an IPv4 address on the LAN as an SSDP sender is:
    `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` or link-local `169.254.0.0/16`,
    never loopback or `0.0.0.0/8`), no search is needed. Otherwise, or for another
    zone, the search runs and the file is updated. A file that can't be read, is
    not a regular file (a symlink or a FIFO is not opened through), is larger
    than 4 KiB or holds no valid location (plain `http://` on its stored host) is
    ignored with a warning. A state directory writable by group or others is
    warned about at start: whoever can write there chooses the location the
    adapter tries first (still checked as above); make it private to the
    adapter. A file that can't be written is logged and the adapter goes on with
    its in-memory cache. Without `ADAPTER_STATE_DIR` nothing is written. The host
    itself is still found by SSDP without `RAUMFELD_HOST`.
  - Either way: a host whose `getZones` doesn't list `RAUMFELD_ROOM_UDN` is an
    error (a wrong host), and a renderer description whose `UDN` is not the zone
    UDN is refused: never a wrong device. If nothing is found at start, that is
    logged and the first cast looks again.
  - The start gate's renderer looks (every 0.25 s) use the cached renderer and
    don't look for it again when one fails; the next takeover or room check does.
- **Start gate:** the speaker needs several seconds to wake, join its zone and
  buffer, while MPD starts the song right away. So when MPD starts playing and
  the adapter doesn't own the room (a new cast, or a resume after the room was
  taken away), it first sends MPD `pause 1`, which holds the song where MPD has
  got to by then, a moment after its start. Then it wakes the room and starts
  the stream as usual. **What happens next depends on the stream:**
  - **FLAC** (`STREAM_MIME` `audio/flac`): MPD's FLAC stream carries no data at
    all while MPD is paused, so a speaker waiting for it would only start when
    the gate gives up. MPD is released (`pause 0`) as soon as the takeover
    returned, logged as `MPD released right after Play`; the speaker then
    starts within about 1 s (measured live). The re-takeover and restart rules
    below don't apply; instead a renderer not yet seen playing gets one more
    takeover before a stopped renderer counts as taken away, and one stuck in
    `TRANSITIONING` gets the usual restarts (`STUCK_SECONDS`).
  - **Other formats** (MP3): MPD's httpd output keeps streaming silence while
    paused (`always_on`), so the speaker can connect. The adapter asks the zone
    renderer every 0.25 s whether it is `PLAYING` on `STREAM_URL` and sends
    `pause 0` as soon as it is. **Re-takeover:** if the renderer has not
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
  on the silence and the song replaces it later (a new `songid`, no stop). If
  MPD is paused then (the gate holds the silence), the receiver hands the song
  over held: `playid <song>; pause 1; deleteid <silence>` in one command list,
  so MPD reports `pause` with the new `songid` and sends no audio of the song;
  the gate carries on with it, held at 0:00, without another `pause 1`, and the
  adapter's `pause 0` starts it once the speaker plays. This also holds when the
  song arrives while a slow takeover blocks the adapter: it reads the change
  after the takeover returned. If MPD plays then, the receiver goes from the
  silence straight to playing the song, and the adapter sends `pause 1` again as
  soon as it reads that change: the song is held a moment after its start, not
  at 0:00 (YouTube streams in MPD can't be sought back to 0:00). Either way the
  gate's timing and re-takeover count are unchanged, and there is no new
  takeover. A skip from the phone during the gate is held the same way (holding
  at the start of the new song undoes nothing). If the song arrives while the
  adapter's `pause 0` is on its way, the speaker already plays: played by MPD,
  the gate ends and MPD plays the song; handed over held (the receiver saw MPD
  paused just before the `pause 0` landed), the adapter sends `pause 0` again.
  That race can also show only after the gate ended, the `pause 0` having landed
  on the silence: a `pause` with a new `songid` at most 1 s into the song
  (`HANDOVER_ELAPSED_S`), within 5 s of the gate's end (`AFTER_GATE_HANDOVER_S`),
  is taken for the receiver's held hand-over and resumed too. The window is short
  on purpose: the receiver keeps the phone from pausing during the pre-roll, and
  right after the hand-over the phone can't have paused a song it hasn't played
  yet; any other pause is left alone, as is a paused new song after a gate in
  which a song already replaced the silence (a hand-over only replaces the
  silence). If the song arrives after the gate while
  MPD plays (the speaker already plays the silence), there is no gate and no
  takeover: the stream just goes on with the song. If MPD stops during the gate
  (the phone, or the receiver ending the silence after 60 s), the adapter does
  not unpause. **Limit:** a pause from the phone during the gate can't be
  told apart from the adapter's own pause (MPD is already paused, so nothing
  changes), and is undone when the gate opens. Song changes while the adapter
  owns the room are not gated: the speaker already plays the stream. The phone
  shows the position standing still during the gate.
- **Pause and resume from the phone reach the speaker.** The Raumfeld buffers
  5-6 s of the stream, so a pause done only in MPD would be heard that much
  later. On a user pause (MPD play -> pause with the same song, on a zone the
  adapter owns; never the gate's own `pause 1`, nor the receiver's held
  hand-over, which comes with a new song) the adapter asks the zone renderer
  (`GetTransportInfo`/`GetMediaInfo`) and, if it plays `STREAM_URL`, sends
  AVTransport `Pause` right away and logs it. While MPD stays paused, a renderer
  `PAUSED_PLAYBACK` on our stream is the adapter's own pause: no takeover, no
  restart, no yield (standby, another source or leaving the zone still count as
  taken away, as before). On the resume of the same song (MPD pause -> play) it
  asks the renderer again: still `PAUSED_PLAYBACK` on our stream, it holds MPD
  at the resume point (`pause 1`; a resume gate, `START_GATE_SECONDS=0`: just
  `Play`), looks at the renderer once more right before `Play` and sends it (the
  Raumfeld reconnects to the stream and its position starts at 0:00), then
  releases MPD as the start gate does: with FLAC right after `Play`, otherwise
  once the renderer plays. Found playing another source at either look (chosen
  during the pause, before a poll saw it), the speaker is not taken back: the
  adapter yields (pauses MPD, releases the cast). Found otherwise, or if `Play`
  fails, the adapter looks at the room (`getZones`, transport, URI) and yields
  if it was taken away (standby, no zone, another source), or takes it over,
  still behind the gate; a failed look releases MPD and leaves it to the polls.
  At the first look (MPD has resumed) anything other than paused on our stream
  is a resume as without the pause. **Where the resume continues:** the speaker
  drops what it had buffered when `Play` reconnects it, so MPD is that far
  ahead of what was heard. At the pause the adapter measures it, as the time
  since its `Play` connected the speaker less the renderer's `RelTime`
  (`GetPositionInfo`, whole seconds, so less 0.5 s more); a value outside
  0.1-3 s, or none, counts as 1.0 s (measured live: 0.8-1.0 s). At the resume
  of the same song, still held, MPD is set back by it (`seekcur -<s>`; it stays
  paused) before `pause 0`. A seek MPD refuses (`ACK`, e.g. a song that can't be
  sought yet) is logged and the resume goes on; one that takes longer than 2 s
  ends the MPD connection, which is reconnected without the seek. No seek for
  another song, a speaker that left the pause, or without the gate. Another
  song while the speaker is paused (the phone picks or skips one) is a start,
  through the start gate, not a resume. A pause then stop (the session ended)
  sends no `Play`, and the adapter lets go of the room (stops polling): the
  next play is a start. A pause done on the speaker itself while MPD plays is
  not the adapter's: it counts as a stopped renderer, as before. A failed
  `Pause` is logged; the speaker then plays its buffer out.
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
  song; not the pre-roll's song replacing the silence, which is the receiver's
  doing, within 60 s of the gate's start), drops that exception, so a user who
  stops the speaker later is still respected. If the speaker counts as taken
  away while MPD is playing or paused, the adapter:
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
- **Volume: the phone's slider is the room's own volume**, scaled so that
  phone 100 % is `MAX_VOLUME` (default 50; the speaker's real maximum would be
  far too loud). MPD's volume is just a number: the httpd output needs
  `mixer_type "null"` (as in [`mpd.conf.example`](mpd.conf.example); without a
  mixer MPD answers the phone's volume change with `ACK ... {setvol} No mixer`,
  and `"software"` would scale the samples), and the adapter waits for
  `idle player mixer`. While it owns the zone and MPD plays or is paused, a
  change of MPD's volume is sent at once to the zone renderer as
  RenderingControl `SetRoomVolume` for `RAUMFELD_ROOM_UDN` (the room's own
  volume; never the zone's `SetVolume`, which would scale every room in the
  zone): `round(v * MAX_VOLUME / 100)`, half up, never above `MAX_VOLUME`. The
  other way, the room's volume (`GetRoomVolume`) is given
  to MPD as `min(100, round(speaker * 100 / MAX_VOLUME))`: at each takeover of a
  zone the adapter did not own (the phone's volume is never pushed to the
  speaker then, so the loudness does not jump; a speaker louder than
  `MAX_VOLUME` shows 100 % and is left alone until the slider moves) and at each
  look at the room (`SPEAKER_POLL_SECONDS`), where a change made on the speaker
  itself is noticed. MPD's report of the adapter's own `setvol` is not sent back
  (no feedback loop), and the round trip maps back onto the same speaker volume.
  The receiver tells the phone (`onVolumeChanged`) when MPD's volume changes
  other than by the phone. No volume requests without a cast, on a zone taken
  away or yielded, with MPD stopped, or from the start gate's checks, restarts
  and re-takeovers. A failing volume request is logged and changes nothing
  else (after a failed `SetRoomVolume` the next look gives MPD the room's
  volume again, so the phone shows what plays). A renderer description without
  RenderingControl, or with its control URL on another host, only disables
  volume, without a warning at each look.
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
| `RAUMFELD_HOST` | unset | IPv4 address or DNS name of the Raumfeld host (no port, scheme or path), e.g. `<raumfeld-host>.local`. Reaches its API directly (no SSDP for the host) and takes only that address's answer to the multicast search for the zone renderer (the Raumfeld doesn't answer unicast SSDP). The room and zone UDNs are still checked: a wrong host is an error. Must resolve to the address the host answers SSDP from; an IPv4 address binds firmest (no DNS) |
| `ADAPTER_STATE_DIR` | unset | Absolute path of an existing, writable directory (e.g. `/data` in a container) where the zone renderer's location is kept across restarts, so a restart needs no search. Unset: in-memory cache only |
| `STREAM_URL` | required | MPD httpd stream (`http://` or `https://`), e.g. `http://<receiver-host>:8000/stream.mp3` |
| `STREAM_MIME` | `audio/mpeg` | MIME type announced in the DIDL-Lite metadata |
| `STREAM_METADATA` | `1` | `0` sends empty `CurrentURIMetaData` instead of DIDL-Lite |
| `SPEAKER_POLL_SECONDS` | `5` | How often (seconds, at least 1) the room is checked while the adapter owns the zone |
| `START_GATE_SECONDS` | `15` | How long (seconds, 0-60) MPD is held at the start of the song while the speaker wakes; `0` turns the start gate off. A takeover in the gate always gets 5 s to start playing, so the hold can run past this value, but never past 60 s: holding the song any longer only leaves the phone standing still |
| `MAX_VOLUME` | `50` | The speaker volume (1-100) at phone 100 %; the phone's volume is scaled linearly to 0..`MAX_VOLUME`, never above it |
| `STUCK_SECONDS` | `15` | How long (seconds, 1-300) the renderer may stay `TRANSITIONING` on the stream while MPD plays before it is restarted with `Stop`, `Play` |
| `MPD_HOST` / `MPD_PORT` | `localhost` / `6600` | MPD control connection |
| `LOG_LEVEL` | `INFO` | `DEBUG`, `INFO`, `WARNING`, `ERROR`, `CRITICAL` |

Missing or invalid settings make the adapter exit with status 2 and a message
naming the variable. `IDLE_MINUTES` is no longer used; if it is set, the
adapter logs a warning and ignores it.

See [`mpd.conf.example`](mpd.conf.example) for a matching MPD setup (httpd
output on port 8000, MP3 or FLAC, null mixer for the phone's volume).

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
then replaces the silence, during the gate (held until the speaker plays: at
0:00 when the receiver hands it over while MPD is held, a moment after its
start if MPD was playing) or after it (it plays at once, the speaker already
plays the stream).
Either way there is only one takeover. If no song comes, the receiver stops the
silence after 60 s, which also ends a gate still running.

Each cast start is logged at INFO, one line per phase, times from the moment MPD
started playing (the gate's start), e.g.:

```
start: takeover returned at +2.6 s (wake wait 2.1 s, cached addresses, no discovery)
start: song '2' at +2.6 s, speaker not playing yet: held
start: speaker plays at +4.3 s; resuming MPD
start: MPD resumed at +4.3 s (song '2' held 1.7 s)
```

Here the receiver handed the song over (held) while the takeover still waited for
the room; the adapter reads that change once the takeover returned, so the song's
time is never earlier than the takeover's. A song after the gate shows as
`start: song '2' at +6.0 s, speaker already playing: no hold`, one handed over
held right after the gate as `held by the receiver after MPD resumed: resuming
it`. `1 discovery` or `N discoveries` instead of `cached addresses` says the
takeover had to look for devices (the host by SSDP or by resolving a
`RAUMFELD_HOST` name, the zone renderer) first; an IPv4 `RAUMFELD_HOST` is no
lookup.

## Tests

```sh
python3 -m unittest discover -s adapter -v
```

The tests use local fake servers (MPD, Raumfeld host API, device description,
AVTransport SOAP, SSDP responders standing in for the multicast group) on
127.0.0.1 and need no real devices; no multicast leaves the machine.
