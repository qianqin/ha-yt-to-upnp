# Changelog

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
