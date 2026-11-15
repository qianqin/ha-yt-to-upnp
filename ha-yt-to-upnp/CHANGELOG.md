# Changelog

## 0.4.3

- PoTokens YouTube accepts: the PoToken helper minted tokens YouTube
  rejected (SABR `stream_protection_status=2`, throttled after about 60 s),
  for two reasons. It ran without the Supervisor's `TZ`, so with the image's
  UTC time zone (offset 0); and bgutils-js fetched BotGuard with a fixed
  macOS user agent while the challenge saw the real platform
  (`Linux x86_64`). The helper now gets Home Assistant's time zone as `TZ`
  and starts through `/opt/bgutil/ytcast-entry.ts`, which sends a user agent
  in bgutils-js's format naming the real platform; neither the platform nor
  the time zone is faked, and Deno's permissions are unchanged. With
  Europe/Berlin it mints 120-character tokens, the kind YouTube accepts
  (124 before). At start the log shows the helper's time zone and UTC offset,
  with a warning when there is no time zone or its offset is 0.

## 0.4.2

- Premium audio for the whole queue: a cast token (ctt) is valid for one
  song only, and the phone sends one only for the song it starts. When the
  receiver advanced to the next song by itself it reused the first song's
  token, YouTube then offered no Premium formats and the song played as 251
  via SABR. While a song plays, the receiver now asks InnerTube `/next` (as a
  TV, with the connected phone's device id, as yt-cast-receiver does) for the
  next queue song and its own token, at most one song ahead, and plays and
  reports each song with its own token and params. If `/next` gives no
  token, fails, or names another song than the queue's next, that song plays
  without a token (251 via SABR); another song's token is never used. Next
  and previous from the phone use the same per-song tokens. Tokens stay in
  memory only, go only to YouTube, and are forgotten with a new cast, a
  sender connecting on the other YouTube theme, a stop from the phone, a
  disconnect or a release by the speaker; a `/next` answer that arrives
  after that is dropped. `/next` gives up after 10 s. `YTCAST_DEBUG_NEXT=1`
  logs the key structure (no values) of each `/next` answer.
- A SABR stream that stalls (YouTube stops sending audio after about 60 s
  when it does not accept the PoToken, `stream_protection_status=2`) no
  longer ends the song there: the receiver fetches a fresh `/player`
  response and a fresh PoToken and continues from the reached position in
  the same stream to MPD, at most 3 times per song, then gives up as before.
  This also works for songs streamed without ctt (anonymous IOS client).

## 0.4.1

- Pause from the phone is heard at once: the adapter follows a user pause to
  the speaker (UPnP AVTransport `Pause` on the zone renderer) instead of
  letting it play its 5-6 s buffer out. A resume sends `Play` and sounds
  after about 1 s, where the pause was heard: the speaker drops what it had
  buffered, so the adapter measures that at the pause and sets MPD back by it
  (about 1 s) before releasing it. A speaker switched to another source
  during the pause is not taken back; one that stopped during the pause (a
  stop the poll had not yet seen) is taken over as before. Picking another
  song while paused starts it like a new cast.
- Faster start with FLAC: MPD's FLAC stream carries no data while MPD is
  paused, so the speaker could only start when the start gate gave up (about
  20 s at every start). With FLAC the adapter now releases MPD right after
  telling the speaker to play; it starts in about 1 s. MP3 keeps holding the
  song until the speaker plays.
- Volume from the phone sets the room's own volume (UPnP RenderingControl
  `SetRoomVolume` on the room's zone renderer; other rooms in the zone keep
  theirs), at once instead of after the speaker's buffer, scaled so that
  100 % on the phone is the new option `max_volume` (1-100, default 50),
  never louder. MPD plays at a fixed 100 % (`mixer_type "null"`, the FLAC
  stream stays bit-perfect). At a cast start the speaker keeps its volume and
  the phone is shown it; a volume changed on the speaker itself shows on the
  phone within about 5 s. Replaces "the speaker's volume is never changed".
- Artefacts stage: the test-only `ARTEFACTS_URL` is checked as a whole and
  as one line; before, a second line or a backslash escape such as `\c`
  (which `echo` in dash acts on) left part of the value unchecked, so user
  info could slip into an https URL. The build from the repository never sets
  it.
- Artefacts stage: the 512 MiB cap now also holds for a download without a
  Content-Length (bookworm's curl 7.88 counted only that header); curl runs
  with `-q` (no `.curlrc`) and `--globoff`, an https `ARTEFACTS_URL` may not
  contain `[]{}` or a backslash, the pattern checks run with `LC_ALL=C`, and
  URLs are printed as they are (printf, not echo).
- Tests: `scripts/fetch_artefacts_test.py` (URL check, sha256 in the lock,
  512 MB cap, no setuid/setgid bits) and `scripts/ha_mpd_conf_test.py`
  (exactly one `@STREAM_ENCODER@` in `mpd.conf.template`, now checked by
  `ytcast_mpd_conf` in `common.sh`; every `stream_format` has an encoder and
  a MIME type, and MPD serves each with it), both run by
  `scripts/smoke-ha-app.sh`;
  more cases for `scripts/publish-release.sh` and `scripts/pack_artefacts.py`.
- `ytcast_mpd_conf` puts the encoder settings in as they are (quoted
  replacement: bash 5.2 would turn an `&` into the placeholder).

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
