# YouTube to UPnP

Makes a Teufel Raumfeld room appear in the **YouTube Music** cast menu. The
app bundles four processes:

| Process | What it does | Listens on |
| --- | --- | --- |
| Receiver (`ytcast-lite`) | Cast target for YouTube Music (DIAL + YouTube Lounge API), streams the audio into MPD; plays silence into MPD while a cast loads (`/silence`, answered on loopback only) | `8008/tcp` (DIAL, LAN), `1900/udp` (SSDP multicast, LAN) |
| MPD | Plays the audio and re-serves it as a WAV (48 kHz, 16-bit), FLAC or MP3 (320 kbit/s) stream, see [Stream format](#stream-format) | `8000/tcp` (stream, LAN), `127.0.0.1:6600` (control, loopback only) |
| PoToken helper (bgutil-ytdlp-pot-provider on Deno) | Proof-of-origin tokens, so YouTube does not throttle streams after a minute | `127.0.0.1:4416` (loopback only) |
| Raumfeld adapter | Wakes the room and makes it play MPD's stream when a cast starts; ends the cast when the speaker is switched off or to another source | none (SSDP/UPnP client) |

The app uses the host network: SSDP/DIAL discovery and the Raumfeld's UPnP
need multicast on the LAN. All four ports above are therefore ports **of the
Home Assistant host**. Nothing else on the host may use `8000`, `8008`,
`6600` or `4416` (another MPD app, for example).

The app has no access to Home Assistant's API or configuration: none of its
processes can read the Supervisor token that its container is given. It stores
no credentials: each cast brings a short-lived token from the phone. The only
persistent state is under the app's `/data`: the receiver's device identity
and decoder cache (`/data/ytcast`), MPD's state (`/data/mpd`, with the last volume) and
where the adapter last found the room's zone renderer (`/data/adapter`, no
secrets), so a restart needs no search for it.
The volume from the phone sets the speaker's own volume (UPnP RenderingControl
on the room's zone renderer), scaled so that 100 % on the phone is
`max_volume`, never more; MPD itself plays at full volume (the stream is left
untouched). See [Volume](#volume).

## Before you start

- **Only one receiver with the same cast name on the LAN.** Stop any other
  instance (a test setup on another computer, for example) before starting the
  app; two devices with the same name confuse the phone.
- Find the UDNs of the Raumfeld room and of the speaker device that hosts the
  Raumfeld API (UPnP device descriptions or the Raumfeld host API's
  `getZones`). UDNs look like `uuid:00000000-0000-0000-0000-000000000001`.
- Optional, for a faster start: the LAN IPv4 address of that speaker device
  (the **Raumfeld host**), see [Finding the Raumfeld host](#finding-the-raumfeld-host).

## Installation

Home Assistant installs the app from this repository and builds its image
itself, on the Home Assistant host:

1. Settings → Apps → **Repositories**: add
   `https://github.com/qianqin/ha-yt-to-upnp` and close the dialog.
2. The app **YouTube to UPnP** appears in the app store (reload the page if
   not). Open it and **Install**. The build takes a few minutes; nothing is
   compiled: it installs MPD and Python from Debian (apt) on the pinned base
   image, downloads the prebuilt programs (receiver, adapter, Deno and the
   PoToken helper) as one tarball from the GitHub release of the app's version
   over https, and uses them only if their SHA-256 is the one in the
   repository's `artefacts.lock`. On any mismatch the build stops and the
   app is not installed.
3. **Configuration** tab: set the [options](#options), save, then **Start**
   on the **Info** tab. Check the log for the four `Starting ...` lines (see
   [Use](#use)).

**Updates:** when a new version is in the repository, the app's page offers
**Update** (Settings → Apps, or Settings → Updates). It rebuilds the image
the same way and restarts the app; the options and `/data` are kept. The
changelog of each version is on the app's page and in `CHANGELOG.md`.

The app is built for x86-64 (`amd64`) hosts only. A folder assembled with
`scripts/build-ha-app.sh` (it brings the programs along in `artefacts/`) can
still be installed as a local app instead; see `docs/DEVOPS.md` in the
repository.

## Options

| Option | Required | Meaning |
| --- | --- | --- |
| `cast_name` | yes | Name in the cast menu. Up to 60 characters, no `<`, `>`, `&` or `"`. |
| `raumfeld_room_udn` | yes | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001`. |
| `raumfeld_speaker_udn` | without `raumfeld_host` | UDN of the speaker device that hosts the Raumfeld API; the adapter finds it on the LAN by an SSDP search for this UDN. Not needed with `raumfeld_host` (checked if set). |
| `raumfeld_host` | no | IPv4 address (recommended) or DNS name of the Raumfeld host, without port, `http://` or path, e.g. `192.0.2.20`. The adapter reaches its API directly instead of searching the LAN for it, so a cast starts sooner. It must be the address the host answers SSDP from (its address on the speakers' LAN): the room's zone renderer is still found by a multicast search, and only answers from this address count. A host that doesn't list `raumfeld_room_udn` is an error, never a wrong device. A DNS name is only as trustworthy as the LAN's name resolution (mDNS, the router's DNS); use the fixed IPv4 address where possible. Empty: found by `raumfeld_speaker_udn`. |
| `stream_host` | no | Host name or IPv4 address the speaker fetches the stream from (`http://<stream_host>:8000/stream.wav`, or `stream.flac`/`stream.mp3` with `stream_format` `flac`/`mp3`). Empty: the host's LAN IPv4 address (source address of the default route), detected at each start. Set it if the host has several networks or the detected address is wrong. |
| `start_gate_seconds` | no | 0-60, default 15. How long the song is held at its start while the speaker wakes; `0` turns it off. |
| `stuck_seconds` | no | 1-300, default 15. How long the speaker may hang while buffering before it is restarted. |
| `max_volume` | no | 1-100, default 50. The speaker's volume at 100 % on the phone; the phone's volume is scaled linearly to 0..`max_volume` and never sets the speaker louder. See [Volume](#volume). |
| `stream_format` | yes | `wav` (default), `flac` or `mp3`: the format of the stream the speaker plays, see [Stream format](#stream-format). `flac` is slow to start and resume (about 20 s); it stays accepted for installs that have it set. |
| `log_level` | yes | `debug`, `info` (default), `warning` or `error`, for the receiver and the adapter. |

The app checks the options again at each start. If one is missing or invalid,
it starts nothing and stops, with one error in the log naming the option and
the rule it breaks (for example `option 'cast_name' is invalid: must match
...`, or `option 'raumfeld_speaker_udn' is required when raumfeld_host is not
set`). Fix the option and start the app again.

### Volume

The phone's volume slider moves the speaker's own volume at once (no wait for
the speaker's buffer): phone 0-100 % is speaker volume 0..`max_volume`,
linear. At a cast start the speaker keeps its volume and the phone is shown
it (speaker volume × 100 / `max_volume`), so the loudness does not jump; a
speaker louder than `max_volume` shows 100 % and is left as it is until you
move the slider. A volume changed on the speaker itself (its buttons, the
Raumfeld app) shows on the phone within about 5 s. The app
only touches the volume while it plays on the speaker (not on another
source, in standby or with no cast).

### Stream format

MPD re-encodes the song (YouTube's Opus, or AAC if no Opus is offered; lossy
already) into the stream the speaker fetches on port `8000`. All formats are
48 kHz, 16-bit stereo. 48 kHz is Opus's own rate, so Opus needs no
resampling; AAC (44.1 kHz) is resampled to 48 kHz. The rate is fixed on
purpose: whenever the stream's sample rate changes, MPD's HTTP stream closes
the speaker's connection, so following each song's own rate would cut the
stream between a 44.1 kHz and a 48 kHz song.

| `stream_format` | Stream | URL | MIME type | On the LAN |
| --- | --- | --- | --- | --- |
| `wav` (default) | WAV (PCM), lossless | `/stream.wav` | `audio/wav` | about 1.5 Mbit/s |
| `flac` | FLAC, lossless, slow to start and resume | `/stream.flac` | `audio/flac` | about 1-1.5 Mbit/s |
| `mp3` | MP3 (LAME), 320 kbit/s CBR | `/stream.mp3` | `audio/mpeg` | 320 kbit/s |

- **WAV** adds no second lossy step and no encoding: the speaker gets exactly
  what MPD decoded, 16-bit/48 kHz PCM, about 1.5 Mbit/s on the LAN. While the
  app holds MPD paused (the start gate at a cast start, and the resume after
  a pause) the WAV stream carries silence, so the speaker connects and plays
  within about 2 s. **Length of a connection:** MPD's WAV header announces
  about 23 minutes of data (the stream itself goes on), and the Raumfeld
  plays just that much, then closes the connection (after the third time it
  stays stopped). The app reads that length from the stream and gives the
  speaker a new connection before it runs out: at the first song change after
  about 17 minutes it holds the new song at its start for a few seconds (the
  speaker plays out the end of the song before), sets the stream again and
  goes on once the speaker plays, a gap of a few seconds between two songs.
  A single song longer than that (or a speaker that stops at the length
  anyway) is taken over again as soon as the app sees it stopped: a short gap
  within the song.
- **FLAC** is lossless too, but while MPD is paused its stream carries no data
  at all, so the speaker stays `TRANSITIONING` until the app's start gate gives
  up: about 20 s at every cast start and every resume. It is still accepted so
  that installs which have it set keep working; switch to `wav`. It is
  **16-bit, not 24-bit**, on purpose: a Raumfeld connects to a 24-bit FLAC
  stream but never starts playing it.
- **MP3** is the fallback: use it if the network to the speaker (Wi-Fi with a
  weak signal, for example) drops out with WAV's bandwidth. It also carries
  silence while paused.

The change takes effect when the app is restarted. The adapter announces the
stream to the speaker with the matching MIME type.

### Finding the Raumfeld host

The Raumfeld host is the speaker device whose UDN is `raumfeld_speaker_udn`;
it serves the Raumfeld API on port `47365`. To find its address:

- Start the app once without `raumfeld_host` and with `log_level` `info` or
  `debug`: the adapter finds the host by its UDN and logs `Raumfeld host at
  http://<address>:47365/, zone renderer ...` (an info line). Use that
  `<address>`.
- Or look the speaker up in your router's list of network devices.

Check it in a browser on the same LAN: `http://<address>:47365/getZones`
lists the rooms, among them `raumfeld_room_udn`. Give the speaker a fixed
address (a DHCP reservation in the router), so that it keeps the address; if
it changes, the adapter finds nothing at the old one and logs that (see
Troubleshooting).

## Use

1. Set the options and start the app. The log shows each process starting:
   `Starting MPD`, `Starting PoToken helper`, `Starting receiver as '<name>'`,
   `Starting Raumfeld adapter (stream http://<host>:8000/stream.wav)`
   (`stream.flac`/`stream.mp3` with `stream_format` `flac`/`mp3`).
2. On the phone (same LAN), open YouTube Music, tap Cast and pick the name.
3. While the cast loads, the receiver already plays silence into MPD, so the
   adapter wakes the room and connects it to the stream before the song is
   there. The song then takes over the stream, held at its start (up to
   `start_gate_seconds`) until the speaker plays.

Each process is restarted by the app's supervisor if it exits; the log says
which one and with which exit code.

## Troubleshooting

- **Not in the cast menu:** phone and Home Assistant must be on the same LAN
  segment (multicast). Check the log for `SSDP listening`. Another device with
  the same name or another process on port `8008` also prevents it.
- **Speaker stays silent:** check the adapter's log lines. The speaker must
  reach `http://<stream_host>:8000/stream.wav` (or `.flac`, `.mp3`); set
  `stream_host` if the detected address is wrong.
- **Speaker connects but never plays** (the adapter's log shows it
  `TRANSITIONING` and restarting it): with `stream_format` `flac` that is
  expected for about 20 s at each start and resume, set it to `wav`; otherwise
  set it to `mp3` and restart the app.
- **`Raumfeld room not found yet` in the log:** the adapter found no Raumfeld
  host at start, the host doesn't list the room, or the host was found but
  the room's zone renderer wasn't; the cause is in brackets (for the zone
  renderer `DeviceNotFound("no SSDP answer for '<zone UDN>'")`). It keeps
  running and looks again at the first cast. With `raumfeld_host` set, check that the
  address is still the speaker's (see
  [Finding the Raumfeld host](#finding-the-raumfeld-host)) and that
  `getZones` there lists `raumfeld_room_udn`; or clear `raumfeld_host` to
  search by `raumfeld_speaker_udn`.
- **Stream stops after about a minute:** the PoToken helper is not running;
  look for its exit in the log.
- **App stops right after starting:** an option is missing or invalid; the
  log names it.

## Known limitations

- **MPD's control port has no password.** "Loopback only" means the loopback
  of the Home Assistant host (host network): Home Assistant itself and any
  other app with host networking can connect to `127.0.0.1:6600` and control
  playback, the queue and the volume (and through the adapter the speaker's
  volume, at most `max_volume`). Neither the receiver nor the adapter can
  send an MPD password yet. Nothing outside the host can reach the port.
