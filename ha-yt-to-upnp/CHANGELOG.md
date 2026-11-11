# Changelog

## 0.4.1

- Pause from the phone is heard at once: the adapter follows a user pause to
  the speaker (UPnP AVTransport `Pause` on the zone renderer) instead of
  letting it play its 5-6 s buffer out. On resume it sends `Play` and holds
  MPD at the resume point until the speaker plays again (like at a cast
  start), so nothing is lost while the speaker reconnects. A speaker switched
  to another source during the pause is not taken back; a speaker that left
  the pause (standby, stopped) is taken over as before.
- Volume from the phone sets the speaker's own volume (UPnP RenderingControl
  on the room's zone renderer), at once instead of after the speaker's buffer,
  scaled so that 100 % on the phone is the new option `max_volume` (1-100,
  default 50), never louder. MPD plays at a fixed 100 % (`mixer_type "null"`,
  the FLAC stream stays bit-perfect). At a cast start the speaker keeps its
  volume and the phone is shown it; a volume changed on the speaker itself
  shows on the phone within about 5 s. Replaces "the speaker's volume is never
  changed".
- New default `stream_format: wav` (uncompressed 16-bit/48 kHz PCM, about
  1.5 Mbit/s on the LAN, still lossless). With FLAC, MPD sends no data while
  it is paused, and the app holds it paused at every cast start and resume
  until the speaker plays, so the speaker waited for the gate to give up
  (about 20 s); WAV and MP3 carry silence then, and the speaker starts in
  about 2 s. `flac` stays accepted (installs that have it saved keep
  working) but is documented as slow; switch to `wav`.
- WAV connections are renewed before they run out: the Raumfeld stops after
  the data length MPD's WAV header announces (about 23 minutes). The adapter
  reads that length from the stream and, at the first song change after
  about 17 minutes, holds the new song for a few seconds while the speaker
  plays out its buffer, sets the stream again and resumes once the speaker
  plays. A speaker that stops at the length anyway (a song longer than that)
  is taken over again behind the start gate instead of ending the cast.

## 0.4.0

Premium audio, installed from the repository.

- Premium audio for casts from a YouTube Music Premium account: the receiver
  asks with the phone's cast token for format 774 (Opus, about 257 kbit/s),
  then 141 (AAC 256 kbit/s), both through the deciphered format URL, and
  falls back to 251 (Opus, about 160 kbit/s, SABR). The cast token is used
  only for the cast it came with and never stored.
- Each song keeps the format it started with, so a seek never switches
  format (byte ranges of the same file).
- FLAC 48 kHz 16-bit stays the default stream (`stream_format: flac`),
  MP3 320 kbit/s the fallback (`stream_format: mp3`), see 0.3.0; the stream
  stays at a fixed 48 kHz because MPD's httpd output closes its clients when
  the sample rate changes between songs.
- Install from the repository: add the repository URL in Home Assistant
  (Settings → Apps → Repositories), and Home Assistant builds the app itself
  (base image and MPD via apt, nothing compiled) and offers updates. The
  prebuilt programs (receiver, adapter, Deno, PoToken helper) come from the
  GitHub release `v0.4.0` as one tarball, downloaded over https and checked
  against the sha256 in `artefacts.lock`; a mismatch stops the build. A folder
  assembled by `scripts/build-ha-app.sh` (with `artefacts/`) still builds
  without a download, as a local app.
- Dockerfile: two stages, the first only fetches and checks the artefacts.

## 0.3.0

Lossless stream to the speaker.

- New option `stream_format` (`flac` or `mp3`, default `flac`). MPD's
  stream on port 8000 is now FLAC 48 kHz 16-bit (compression level 5,
  about 1-1.5 Mbit/s, `http://<stream_host>:8000/stream.flac`, announced as
  `audio/flac`) or, with `mp3`, MP3 320 kbit/s 48 kHz (`/stream.mp3`,
  `audio/mpeg`). Was MP3 192 kbit/s 44.1 kHz.
- FLAC is 16-bit because a Raumfeld connects to a 24-bit FLAC stream but
  never plays it (stays `TRANSITIONING`); 16-bit FLAC and MP3 320 kbit/s
  both play (checked on a Raumfeld speaker).
- The stream's sample rate is fixed at 48 kHz (44.1 kHz sources are
  resampled): with the source's own rate (`format "*:16:2"`), MPD's httpd
  output closes its clients whenever the rate changes between songs (MPD
  0.23.12 and 0.24.18, FLAC and MP3 alike).
- MPD's configuration is written to `/run/ytcast/mpd.conf` at each start
  from `/etc/ytcast/mpd.conf.template`; an invalid `stream_format` stops the
  app with one error naming it, like any other option.
- Dockerfile: the PoToken helper (Deno, bgutil) is copied before the
  receiver, adapter and app files, so that a rebuild reuses its layers; the
  image's contents and modes are unchanged.

## 0.2.0

Faster cast start, optional fixed Raumfeld host.

- Silence pre-roll: from the phone's DIAL launch on, the receiver plays
  silence into MPD (`http://localhost:8008/silence`, refused with 403 to
  anything but loopback), so the adapter wakes the room and connects it to
  the stream while the cast is still loading.
- Held hand-over: the song replaces the silence in MPD; if the adapter holds
  the silence while the speaker wakes, the song waits paused and the start
  gate carries on with it, so the song starts at its beginning on the
  speaker.
- The adapter finds the Raumfeld host and the room's zone renderer at start
  and keeps them for the first cast; the start gate asks the renderer every
  0.25 s (was 0.5 s). The wake wait after `leaveStandby` (up to 10 s) is
  unchanged from 0.1.0.
- New option `raumfeld_host` (IPv4 address or DNS name): the adapter reaches
  the Raumfeld API there directly instead of searching for it;
  `raumfeld_speaker_udn` is then optional. Without either, the app stops at
  start with an error naming `raumfeld_speaker_udn`.
- The zone renderer's location is kept across restarts in `/data/adapter`
  (private to the adapter), so a restart needs no search.
- Build inputs: receiver and adapter now come from the same commit as the
  app folder by default (see `artefacts/BUILD-INFO`).
- Docs: the app folder goes into `/local_apps` on the Home Assistant host.

## 0.1.0

First local app build.

- Receiver, MPD (httpd MP3 192 kbit/s on port 8000), PoToken helper and
  Raumfeld adapter in one app on the host network; each process runs as its
  own non-root user with an empty environment and is restarted if it exits.
- Options are checked against the schema at each start; a missing or invalid
  option stops the app with one error naming it.
- The services cannot read the Supervisor token (s6's container environment is
  private to root).
- MPD's HTTP stream serves at most 4 clients.
- Base image `ghcr.io/home-assistant/base-debian:bookworm-2026.10.3`, pinned
  by digest.
- PoToken helper: bgutil-ytdlp-pot-provider 2.0.2 (`26475e9`) on Deno 2.9.8,
  Deno cache filled at build time (`--frozen --cached-only`).
- Default build inputs: receiver `7a699f5`, adapter `556d421`
  (see `artefacts/BUILD-INFO` in the built folder for the exact commits and
  checksums).
- Known limitation: MPD's control port (`127.0.0.1:6600` on the Home Assistant
  host) has no password.
