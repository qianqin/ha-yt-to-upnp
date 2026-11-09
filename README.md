# ytcast-open

> **Known issue (Feb 2026):** YouTube Music has a server-side casting bug affecting certain songs across all cast targets — including official Chromecast, Nest speakers, and Android TV. Some tracks show a loading spinner or "Sorry, something went wrong" when cast, while playing fine locally on the phone. This is not a ytcast-open bug. See: [Reddit thread 1](https://www.reddit.com/r/YoutubeMusic/comments/1rdirhx/sorry_something_went_wrong_when_youre_ready_give/), [Reddit thread 2](https://www.reddit.com/r/YoutubeMusic/comments/1r9zu7v/casting_to_honedevices_stopped_working/). Confirmed by testing with a Pixel 9a casting to both this receiver and a Sony Android TV — same failures on both.

A lightweight YouTube Music cast receiver written in Rust. Makes a Raspberry Pi (or any Linux device) appear as a cast target in YouTube Music. Tap Cast, see your device, play music through MPD.

**~13MB RAM idle. Single binary. One optional sidecar for PoToken.**

Audio streaming powered by [sabr-rs](https://github.com/mthwJsmith/sabr-rs), my Rust implementation of YouTube's SABR protocol.

## How it works

```
YouTube Music (phone)
    | SSDP multicast discovery
    v
ytcast-open (single Rust binary)
    | DIAL protocol (port 8008) - device appears in cast menu
    | YouTube Lounge API - long-poll session for play/pause/seek/skip/volume
    | SABR protocol - YouTube's native streaming, built-in via sabr-rs
    v
MPD (Music Player Daemon)
    | ALSA output                  (or: httpd output -> stream URL
    v                               -> optional Raumfeld adapter -> Raumfeld speaker)
speakers
```

1. Phone discovers the device via SSDP/DIAL (same open protocol Chromecast uses for discovery)
2. Phone connects via YouTube's Lounge API (reverse-engineered cast control protocol)
3. Phone sends a video ID
4. Built-in SABR implementation streams audio directly from YouTube's CDN
5. MPD plays the stream. Position, duration, and state sync back to the phone in real-time

No Node.js. No Python. No yt-dlp. Just one binary.

## Stream resolution - SABR

All audio is streamed using YouTube's **SABR (Server Adaptive Bit Rate) protocol**, the same protocol the official YouTube app uses. The SABR implementation is built directly into the binary using [sabr-rs](https://github.com/mthwJsmith/sabr-rs), our Rust port of the protocol.

Unlike direct URL approaches that break when YouTube changes client policies, SABR is YouTube's own native streaming protocol. It handles format negotiation, server redirects, retries, and backoff automatically.

**Under the hood:**
1. InnerTube `/player` call using WEB_REMIX client + credential transfer token (ctt) from the cast session for authenticated access. When casting from YouTube Music, the phone sends a per-video ctt automatically — no setup needed. Without a ctt (or if the anonymous YouTube Music session failed to load) it uses the IOS client, with ANDROID as a further fallback (some tracks may be restricted without authentication)
2. Picks the highest-bitrate Opus audio format, skipping YouTube Music Premium-only formats (774, 141): a ctt can list them, but the SABR session does not serve them
3. Opens a SABR session with YouTube's CDN using protobuf requests
4. Parses UMP (Universal Media Protocol) binary response frames
5. Extracts and streams audio segments as chunked HTTP to MPD
6. Reports buffered ranges so the server sends the next segments

If the server asks for a fresh `/player` response (`RELOAD_PLAYER_RESPONSE`), the stream ends with a clear error instead of restarting the same session in a loop.

The SABR code adds roughly 2400 lines of Rust across 3 files. Proto definitions (18 `.proto` files) are compiled at build time via `prost-build`. This replaced an earlier Node.js SABR proxy (ytresolve) that used ~84MB RSS on its own.

## Features

- Cast from YouTube Music or YouTube (phone, tablet, browser)
- Play, pause, seek, skip, previous, volume - all controlled from phone
- Speaker wakes early: from a DIAL launch that pairs a phone (one with a `pairingCode`) until the first song, an idle MPD plays silence (pre-roll), so an `httpd` stream and the Raumfeld adapter already wake and connect the speaker while the phone is still loading the song. The song then replaces the silence without MPD stopping in between (`addid`, `playid`, `deleteid`); if that hand-over fails, MPD is stopped and cleared. The silence is never reported to the phone and is no song end: play, pause and seek from the phone leave it alone and are answered with "stopped". A song that is playing or paused is never interrupted, and repeated launches neither stack nor restart it. It stops (MPD `stop` + `clear`) after 60 s without a song, on a speaker release (the phone keeps its cast; its song takes the speaker again when it arrives), a stop from the phone or a disconnect. After a timeout or a release, launches start no silence for 45 s, so repeated launches cannot keep the speaker awake or away from line-in. At most 2 `/silence` streams are served at once
- Auto-advance to next track when a song finishes (MPD idle events, not polling); a track whose audio stream fails is reported to the phone as not started and does not advance the queue
- Playlist support with queue updates mid-session
- Phone UI stays fully synced (seekbar, album art, controls)
- Graceful disconnect (stops playback when phone disconnects)
- Best-quality Opus audio (itag 251, up to ~160kbps, 48kHz stereo)
- Authenticated streaming via cast session credential transfer tokens (WEB_REMIX + ctt)
- Full YouTube Music session context (fetches live ytcfg anonymously from music.youtube.com for WEB_REMIX /player)
- n-parameter (nsig) challenge decoding via embedded yt-dlp/ejs solver (0.8.0) in QuickJS. When YouTube changes its player and streams start failing, an outdated solver is the usual cause - see [src/vendor/README.md](src/vendor/README.md) to update it
- Dual Lounge sessions (YouTube + YouTube Music themes) for broad compatibility

## Requirements

- **MPD** (Music Player Daemon) running on the same host - audio is served to MPD via `http://localhost:8008/stream/...` (and the pre-roll silence via `http://localhost:8008/silence`), which only answer loopback connections
- **Linux** (tested on Raspberry Pi Zero 2W with Debian trixie/aarch64)
- Network access to YouTube servers
- **Optional: [bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs)** for PoToken generation (prevents CDN throttle after 60s). Uses [rustypipe-botguard](https://github.com/tobiashenkel/rustypipe-botguard) internally. Runs as HTTP server on port 4416. ~66MB RAM due to embedded V8 JS engine for BotGuard challenges — can be hosted on a separate machine/VPS to save RAM on the Pi.

No Node.js, no Python, no yt-dlp. (The optional [Raumfeld adapter](adapter/README.md) is a separate Python 3 sidecar.)

## Usage

```bash
# Set device name (default: "Living Room Pi")
export DEVICE_NAME="Living Room Pi"

# MPD connection (defaults: localhost:6600)
export MPD_HOST=localhost
export MPD_PORT=6600

# Optional: keep config and nsig cache in one directory (absolute path)
# (default: OS config dir, nsig cache in OS cache dir under ytcast/)
export YTCAST_DATA_DIR=/data

# Run
./ytcast-open
```

Open YouTube Music on your phone, tap Cast, and select your device.

### Optional: play on a Teufel Raumfeld speaker

Instead of a local sound card, MPD can serve the audio as an HTTP stream (`httpd` output) and the [Raumfeld adapter](adapter/README.md) - a small Python 3 sidecar, standard library only - makes a Raumfeld room play that stream automatically whenever MPD plays. It finds the speaker by UPnP UDN via SSDP (or, with `RAUMFELD_HOST`, at that address with a unicast SSDP search for the zone renderer) once at start, caches what it found, and wakes the room. It never puts the room into standby itself; the Raumfeld's own auto power-off does that. At a takeover it holds MPD at the start of the song (`pause 1`, at most `START_GATE_SECONDS`) until the speaker plays the stream, so the first seconds are not lost. The receiver's pre-roll silence (see Features) starts that takeover at the DIAL launch, about 2 s before the song is ready; the song then replaces the silence within the same gate (held at its start until the speaker plays) or, if the speaker already plays, at once, without a second takeover; MPD's `httpd` output serves nothing while MPD is stopped with an empty queue. A zone renderer that hangs in `TRANSITIONING` while MPD plays is restarted with `Stop`, `Play` (at most 3 times per episode, with backoff). The speaker's own controls win: if the room is switched off (button or auto power-off) or to another source such as line-in during a cast, the adapter pauses MPD and sends `sendmessage ytcast release` on MPD's client-to-client channel, which tells the receiver to end the cast. It then leaves the room alone until the user resumes from the phone. An explicit cast wins at takeover: a new cast or a user resume takes the room over from line-in or any other source, even one reporting playing (line-in stays selected after Home Assistant switches it there and reports playing without audio). It never changes the volume. See [`adapter/mpd.conf.example`](adapter/mpd.conf.example) for the MPD side.

```bash
RAUMFELD_ROOM_UDN=uuid:00000000-0000-0000-0000-000000000001 \
RAUMFELD_SPEAKER_UDN=uuid:00000000-0000-0000-0000-000000000002 \
STREAM_URL=http://<receiver-host>:8000/stream.mp3 \
python3 -m adapter
```

The receiver never logs in and stores no credentials of its own: authentication comes from the casting client, which sends a per-video credential transfer token (ctt) with each track. WEB_REMIX is used only when a cast supplies a ctt and the YouTube Music session loaded; otherwise the anonymous IOS client is used. At startup the public music.youtube.com page is fetched anonymously for the session context (visitor data, signature timestamp, nsig player); that one request carries a fixed consent cookie `SOCS=CAI` ("reject all") so EU visitors are not redirected to the consent page - it holds no account data and is not authentication. Requests carrying the ctt or the session fetch only follow redirects to Google hosts. Cookie files are no longer supported: `YTM_COOKIES` and `ytm-cookies.txt` are ignored, so delete any old `ytm-cookies.txt`.

With `YTCAST_DATA_DIR` set, `ytcast-lite.json` (config) and `nsig-cache/` live under it; otherwise they go to `~/.config/` and `~/.cache/ytcast/` (OS equivalents elsewhere). Config and nsig cache files are written atomically with mode 0600 and created directories with 0700; a cached nsig player that fails to run is retried once with a larger QuickJS memory limit, then base.js is parsed afresh; only a successful re-parse replaces the cache, so a cache is never lost to a failure that hits the re-parse too. nsig cache filenames include the ejs solver version; files from older versions are unused and can be deleted. Use an absolute path for `YTCAST_DATA_DIR` (an empty value counts as unset; a relative path works but logs a warning). If the config or an nsig cache file is readable by group/others, a warning is logged - fix it with `chmod 600 <file>`.

## Building

```bash
# Native build (with mock MPD for development on Windows/Mac)
cargo build --features mock-mpd

# Release build
cargo build --release

# Cross-compile for Raspberry Pi (aarch64)
docker run --rm -v "$PWD:/src" -w /src rust:1.99.1-bookworm bash -c \
  "apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu protobuf-compiler && \
   rustup target add aarch64-unknown-linux-gnu && \
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
   cargo build --release --target aarch64-unknown-linux-gnu"
```

Before a release, run `scripts/release-check.sh`; see [docs/DEVOPS.md](docs/DEVOPS.md).

To run it as a Home Assistant app (receiver, MPD, PoToken helper and Raumfeld adapter in one local app, built on the release machine), see [`ha-yt-to-upnp/DOCS.md`](ha-yt-to-upnp/DOCS.md) and the section *Home Assistant app* in [docs/DEVOPS.md](docs/DEVOPS.md).

## Architecture

| File | Lines | Purpose |
|------|-------|---------|
| `main.rs` | ~1280 | Entry point, player command loop, MPD idle events, auto-advance |
| `lounge.rs` | ~1200 | YouTube Lounge API - session management, streaming long-poll, command parsing (session tokens and pairing codes are kept out of logs) |
| `messages.rs` | ~540 | Lounge message parsing + `ChunkParser` for HTTP streaming responses |
| `music_session.rs` | ~620 | Anonymous YouTube Music session at startup: fetches the public music.youtube.com page for ytcfg context, visitor data, STS and the base.js player for nsig |
| `innertube.rs` | ~250 | InnerTube /player for metadata (WEB_REMIX+ctt with full context, or IOS fallback) |
| `nsig.rs` | ~550 | n-parameter challenge decoder using the vendored yt-dlp/ejs solver (`src/vendor/`) in QuickJS |
| `mpd.rs` | ~810 | Raw MPD TCP client (command + idle connections) + mock mode for dev |
| `ssdp.rs` | ~280 | SSDP multicast discovery responder |
| `dial.rs` | ~770 | DIAL HTTP server on port 8008 + `/stream/:videoId` and `/silence` endpoints (loopback peers only) |
| `preroll.rs` | ~420 | Silence pre-roll from DIAL launch to the first song |
| `config.rs` | ~180 | JSON config (UUID, screen_id, screen_id_m) load/save |
| `storage.rs` | ~350 | Data path resolution (`YTCAST_DATA_DIR`), private 0600 atomic writes, 0700 dirs, loose-permission warnings |
| `sabr/stream.rs` | ~2000 | SABR streaming - InnerTube /player, PoToken fetch, nsig decode, format selection, SABR request loop, UMP parsing. Credentials (PoToken, ctt) are only sent to https `*.youtube.com` / `*.googlevideo.com`; stream URLs or redirects to any other host fail closed |
| `sabr/ump.rs` | ~320 | UMP binary codec (YouTube's custom varint) + streaming parser + tests |
| `sabr/mod.rs` | ~12 | Module glue + protobuf includes |
| `adapter/raumfeld_adapter.py` | ~1060 | Optional Python sidecar (not part of the binary): follows MPD and plays its httpd stream on a Raumfeld room via SSDP/UPnP AVTransport (holding MPD at the song start until the speaker plays, restarting a stuck renderer), and ends the cast when the speaker is switched off or to another source. See [adapter/README.md](adapter/README.md) |

Single-threaded tokio runtime. Two MPD TCP connections (command + idle). One long-poll HTTP connection to YouTube Lounge API. SABR requests run per-track in a spawned task.

## RAM usage

RSS = Resident Set Size (actual physical RAM used by the process).

| Component | RAM (RSS) |
|-----------|-----------|
| ytcast-open idle | ~12.8MB |
| ytcast-open during SABR streaming | ~41MB (~65MB peak during nsig decode with a cached player, settles back; the first decode of a new player briefly peaks around 170MB, QuickJS limit 256MB) |
| bgutil-pot (PoToken sidecar, optional) | ~66MB |
| MPD | ~10MB |
| **Total system (without bgutil-pot)** | **~55-65MB** |
| **Total system (with bgutil-pot)** | **~120-130MB** |

The previous architecture used a Node.js SABR proxy (ytresolve, ~84MB) alongside the Rust binary. That's gone now. All SABR streaming is built into the single binary via [sabr-rs](https://github.com/mthwJsmith/sabr-rs).

The bgutil-pot sidecar generates PoTokens (BotGuard attestation) needed to prevent YouTube's CDN from throttling streams after 60 seconds. It uses ~66MB because it embeds a V8/Deno JavaScript VM to execute Google's BotGuard challenges — this is unfortunately unavoidable as BotGuard requires real JS execution. To save RAM on constrained devices, bgutil-pot can be hosted on a separate machine or VPS and accessed via a single HTTP call.

The log shows whether a PoToken was sent: `stream_protection_status=2 (attestation pending, no PoToken sent …)` means the stream will be throttled after ~60s; running the bgutil-pot sidecar avoids that.

## Comparison

| | ytcast-open (current) | Previous (Rust + Node.js ytresolve) | Typical yt-dlp setup |
|-|----------------------|-------------------------------------|---------------------|
| Idle RAM | **~12.8MB** | ~60MB (3.8MB + 55MB Node) | 50-90MB per invocation |
| Binary/install | **2.4MB single binary** | 2.4MB + 2MB node_modules | Python + yt-dlp + JS runtime |
| Runtime deps | **None** | Node.js 18+ | Python + JS runtime |
| Startup | ~50ms | ~2s (Node.js) | 2-5s per video |
| Stream method | SABR (built-in Rust) | SABR (Node.js googlevideo) | yt-dlp sig decryption |
| Future-proof | Yes - YouTube's own protocol | Yes | Breaks frequently |

## Credits

- [sabr-rs](https://github.com/mthwJsmith/sabr-rs) - our Rust SABR implementation (extracted as a standalone crate)
- [googlevideo](https://github.com/LuanRT/googlevideo) by LuanRT - TypeScript SABR reference implementation + proto definitions
- [yt-dlp/ejs](https://github.com/yt-dlp/ejs) - JavaScript-based n-parameter (nsig) challenge solver, embedded in QuickJS
- [bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs) by jim60105 - Rust PoToken provider (BotGuard attestation)
- [rustypipe-botguard](https://github.com/tobiashenkel/rustypipe-botguard) - Rust BotGuard challenge solver used by bgutil-pot
- [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by patrickkfkan - Node.js cast receiver framework (protocol reference)
- [plaincast](https://github.com/nickvdp/plaincast) - Go cast receiver (Lounge API reference)
- [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1 - original YouTube Cast + MPD bridge

## License

MIT
