# ha-yt-to-upnp

A Home Assistant app that lets you cast from the **YouTube Music** app (and
the YouTube app) to a **Teufel Raumfeld** speaker. The app shows up in the
phone's cast menu like a smart TV would (DIAL + YouTube Lounge API, no Google
Cast certificate needed), fetches the audio from YouTube itself and makes the
Raumfeld room play it over UPnP/DLNA. Casting from a YouTube Music Premium
account gives Premium audio quality, using only the short-lived token each
cast brings from the phone: no login, nothing stored.

## Origins

This is a fork of [ytcast-open](https://github.com/mthwJsmith/ytcast-open) by
**mthwJsmith**: a lightweight Rust cast receiver for YouTube and YouTube Music
that plays into MPD. All credit for the receiver's foundation goes there.

**Kept from upstream:** SSDP/DIAL discovery, the YouTube Lounge API session
(play, pause, seek, skip, volume, queue), the InnerTube `/player` calls with
the cast's credential transfer token (ctt), the built-in SABR/UMP streaming
(from upstream's [sabr-rs](https://github.com/mthwJsmith/sabr-rs)), and nsig
solving with the vendored yt-dlp/ejs solver in QuickJS.

**Changed or added in this fork:**

- **Security hardening.** No stored credentials: OAuth and cookie-file login
  are removed; the only cookie ever sent is the fixed consent cookie
  `SOCS=CAI` ("reject all") on the anonymous music.youtube.com page fetch.
  Debug routes removed, `/stream` and `/silence` answer loopback only,
  credentials only sent to `*.youtube.com` / `*.googlevideo.com`, tokens and
  session IDs kept out of logs, private (0600/0700) state files.
- **PoToken** via bgutil-ytdlp-pot-provider on Deno, loopback only, with
  locked-down Deno permissions (network to three Google hosts, no write, run,
  FFI or sys access), so streams aren't throttled after a minute.
- **SABR reload handling:** `RELOAD_PLAYER_RESPONSE` is answered with a fresh
  `/player` call (same ctt), capped per song.
- **Premium audio:** itag 774 (Opus ~256 kbit/s), then 141 (AAC 256 kbit/s),
  fetched through the deciphered format URL with byte ranges, falling back to
  251 via SABR. Seeks stay on the same format.
- **Fast start:** silence pre-roll from the phone's DIAL launch, so the
  speaker wakes while the song is still loading; the song is handed over held
  at its start until the speaker plays.
- **Raumfeld adapter** (Python, stdlib only): wakes the room and waits until
  it is awake, holds the song in a start gate until the speaker plays, retakes
  a speaker that didn't start, restarts a renderer stuck in `TRANSITIONING`,
  respects standby, line-in and other sources, and releases the cast when the
  speaker is taken away. Never touches the speaker's volume.
- **FLAC 16-bit/48 kHz stream** to the speaker (from 0.4.0, see below).
- **Home Assistant app packaging**, built on the release machine (a Mac) with
  pinned, checksummed inputs, published as release assets.

## How it works

```
YouTube Music / YouTube app (phone)
    |  SSDP discovery, DIAL launch (8008/tcp), Lounge API long-poll
    v
receiver (Rust, ytcast-lite)  <-- audio from YouTube: Premium URL or SABR,
    |                              PoToken from the local helper (127.0.0.1:4416)
    |  http://localhost:8008/stream/<id>   (loopback only)
    v
MPD  --httpd output-->  http://<ha-host>:8000/stream.flac  (or .mp3)
    ^                        |
    |  MPD state (127.0.0.1:6600)  |  fetched by the speaker
    |                        v
Raumfeld adapter  --UPnP AVTransport-->  Raumfeld zone renderer (the room)
```

The phone stays the remote: play, pause, seek, skip and volume go to the
receiver over the Lounge API and become MPD commands. Volume is MPD's software
volume; the speaker's own volume is never changed.

### Audio chain

| Source (from YouTube) | Used when | Stream to the speaker |
| --- | --- | --- |
| itag 774, Opus ~256 kbit/s, 48 kHz | Premium cast, first choice | FLAC 16-bit/48 kHz |
| itag 141, AAC 256 kbit/s, 44.1 kHz | Premium cast, if 774 fails | FLAC 16-bit/48 kHz (resampled) |
| itag 251, Opus ~160 kbit/s, 48 kHz, via SABR | other casts, and the fallback | FLAC 16-bit/48 kHz |

MPD decodes the source and re-encodes it losslessly as FLAC, so there is no
second lossy step; `stream_format: mp3` (320 kbit/s) is the fallback. The rate
is fixed at 48 kHz because MPD's HTTP stream drops its clients when the rate
changes between songs. **Up to 0.2.0** the stream is MP3 192 kbit/s instead
(no `stream_format` option); FLAC arrives with **0.4.0**.

Setting `YTCAST_AUDIO_QUALITY=standard` on the receiver disables Premium
formats (always 251 via SABR).

### What happens when

- **Cast start:** the phone's DIAL launch starts silence in MPD; the adapter
  pauses MPD, wakes the room (`leaveStandby`, waits until it is `ACTIVE`),
  points the zone renderer at MPD's stream and resumes MPD once the speaker
  plays. The song replaces the silence and starts at 0:00 on the speaker.
- **Pause / resume / seek / skip** from the phone: done in MPD; the speaker
  keeps playing the stream. Premium songs are seekable; SABR songs are not.
- **Speaker switched off, auto power-off, or switched to line-in or another
  source:** the adapter pauses MPD and tells the receiver to end the cast. It
  then leaves the room alone until you resume or cast again from the phone.
- **New cast or resume while the speaker is on line-in:** an explicit cast
  wins and takes the room over.
- **Idle:** the app never puts the room into standby; the Raumfeld's own
  auto power-off does.

## Install (Home Assistant)

Home Assistant OS on amd64, with the phone and the speaker on the same LAN
segment (multicast).

**From 0.4.0 (release-based install):**

1. Settings → Apps → App store → ⋮ → Repositories, add
   `https://github.com/qianqin/ha-yt-to-upnp`.
2. Install **YouTube to UPnP**. The Supervisor builds the image on the host
   from the app folder and the prebuilt release assets (nothing is compiled
   on the host).
3. Configure and start (options below).

**0.2.0 and earlier** are installed as a local app: build the folder with
`scripts/build-ha-app.sh` and copy it into `/local_apps` on the Home Assistant
host, see [docs/DEVOPS.md](docs/DEVOPS.md#home-assistant-app).

Main options (full list, rules and troubleshooting in
[ha-yt-to-upnp/DOCS.md](ha-yt-to-upnp/DOCS.md)):

| Option | Meaning |
| --- | --- |
| `cast_name` | Name in the cast menu |
| `raumfeld_room_udn` | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001` |
| `raumfeld_speaker_udn` | UDN of the speaker device that hosts the Raumfeld API; needed unless `raumfeld_host` is set |
| `raumfeld_host` | Optional IPv4 address of that device, e.g. `192.0.2.20`, for a faster start |
| `stream_format` | `flac` (default) or `mp3` (from 0.4.0) |

To find the UDNs, ask the Raumfeld host API: `http://<raumfeld-host>:47365/getZones`
lists the rooms and their UDNs. To find `<raumfeld-host>`, start the app once
with `raumfeld_speaker_udn` and `log_level: info`: the adapter logs
`Raumfeld host at http://<address>:47365/`. Give the speaker a fixed address
in your router.

Run only one receiver with the same cast name on the LAN.

## Security and privacy

- **No credentials stored.** There is no login. Each cast brings a ctt from
  the phone; it lives in memory only, for the songs of that cast, and is
  dropped when the cast ends or another one takes over. No cookies or OAuth
  tokens are read or sent (apart from the fixed consent cookie above). Logs
  show hosts and parameter names, never token values.
- **Persistent state** (under the app's `/data`): the receiver's device
  identity and nsig cache, MPD's state, and where the adapter last found the
  zone renderer. No secrets.
- **Exposed on the LAN** (host network): `1900/udp` SSDP, `8008/tcp` DIAL
  (the phone's launch; `/stream` and `/silence` refuse non-loopback peers with
  403), `8000/tcp` MPD's audio stream (unauthenticated: anyone on the LAN can
  listen to what plays; at most 4 clients).
- **Loopback only:** MPD control `127.0.0.1:6600`, PoToken helper
  `127.0.0.1:4416`.
- Each process runs as its own non-root user with an empty environment and
  cannot read the Supervisor token; the app has no access to Home Assistant's
  API.
- **Known limitation:** MPD's control port has no password. Loopback means the
  Home Assistant host's loopback, so Home Assistant and other host-network
  apps can control playback. Nothing off the host can reach it.

## Development

- Receiver: `cargo build --locked && cargo test --locked` (needs `protoc`);
  toolchain pinned in `rust-toolchain.toml`. `--features mock-mpd` runs it
  without MPD.
- Adapter: `python3 -m unittest discover -s adapter`; see
  [adapter/README.md](adapter/README.md) for its behaviour and settings.
- Release gate: `scripts/release-check.sh` (macOS and Linux-container builds
  and tests, `cargo audit`, adapter tests).
- Home Assistant app: `scripts/build-ha-app.sh` builds the app folder with
  pinned, verified artefacts; `scripts/smoke-ha-app.sh` builds the image and
  checks it on an isolated Docker network.

There is no CI by design: everything is built and tested locally on the Mac
and in temporary Linux containers. Details in [docs/DEVOPS.md](docs/DEVOPS.md).
To update the nsig solver when YouTube changes its player, see
[src/vendor/README.md](src/vendor/README.md).

## Limitations and known issues

- **Premium audio depends on YouTube** keeping the format URLs of 774/141 in
  the WEB_REMIX `/player` answer for cast tokens. If that changes, songs fall
  back to 251 via SABR.
- **No 24-bit FLAC:** a Raumfeld connects to a 24-bit FLAC stream but never
  plays it, so the stream is 16-bit.
- **AAC (141) is resampled** from 44.1 kHz to 48 kHz.
- Some songs fail to cast from YouTube Music on every cast target (a
  server-side YouTube issue reported since early 2026), not only on this one.
- amd64 only; one Raumfeld room per app instance.
- MPD's control port has no password (see above).

## License and credits

ytcast-open is by mthwJsmith; its README states the MIT License, and the
upstream repository ships no LICENSE file. This fork keeps upstream's
copyright and attribution and distributes its changes under the same MIT
terms; see [NOTICE](NOTICE).

Built on the work of:

- [ytcast-open](https://github.com/mthwJsmith/ytcast-open) and
  [sabr-rs](https://github.com/mthwJsmith/sabr-rs) by mthwJsmith
- [googlevideo](https://github.com/LuanRT/googlevideo) by LuanRT (SABR
  reference and proto definitions)
- [yt-dlp/ejs](https://github.com/yt-dlp/ejs) (nsig and signature solver)
- [bgutil-ytdlp-pot-provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider)
  (PoToken helper used in the app) and
  [bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs)
  by jim60105, credited upstream
- [rustypipe-botguard](https://github.com/tobiashenkel/rustypipe-botguard),
  [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by
  patrickkfkan, [plaincast](https://github.com/nickvdp/plaincast) and
  [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1, credited upstream
