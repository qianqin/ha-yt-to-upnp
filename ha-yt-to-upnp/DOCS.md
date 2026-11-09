# YouTube to UPnP

Makes a Teufel Raumfeld room appear in the **YouTube Music** cast menu. The
app bundles four processes:

| Process | What it does | Listens on |
| --- | --- | --- |
| Receiver (`ytcast-lite`) | Cast target for YouTube Music (DIAL + YouTube Lounge API), streams the audio into MPD | `8008/tcp` (DIAL, LAN), `1900/udp` (SSDP multicast, LAN) |
| MPD | Plays the audio and re-serves it as an MP3 stream (192 kbit/s) | `8000/tcp` (stream, LAN), `127.0.0.1:6600` (control, loopback only) |
| PoToken helper (bgutil-ytdlp-pot-provider on Deno) | Proof-of-origin tokens, so YouTube does not throttle streams after a minute | `127.0.0.1:4416` (loopback only) |
| Raumfeld adapter | Wakes the room and makes it play MPD's stream when a cast starts; ends the cast when the speaker is switched off or to another source | none (SSDP/UPnP client) |

The app uses the host network: SSDP/DIAL discovery and the Raumfeld's UPnP
need multicast on the LAN. All four ports above are therefore ports **of the
Home Assistant host**. Nothing else on the host may use `8000`, `8008`,
`6600` or `4416` (another MPD app, for example).

The app has no access to Home Assistant's API or configuration. It stores no
credentials: each cast brings a short-lived token from the phone. The only
persistent state is under the app's `/data`: the receiver's device identity
and decoder cache (`/data/ytcast`) and MPD's state (`/data/mpd`, the volume).
It never changes the speaker's volume: the volume from the phone is MPD's
software volume.

## Before you start

- **Only one receiver with the same cast name on the LAN.** Stop any other
  instance (a test setup on another computer, for example) before starting the
  app; two devices with the same name confuse the phone.
- Find the UDNs of the Raumfeld room and of the speaker device that hosts the
  Raumfeld API (UPnP device descriptions or the Raumfeld host API's
  `getZones`). UDNs look like `uuid:00000000-0000-0000-0000-000000000001`.

## Options

| Option | Required | Meaning |
| --- | --- | --- |
| `cast_name` | yes | Name in the cast menu. Up to 60 characters, no `<`, `>`, `&` or `"`. |
| `raumfeld_room_udn` | yes | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001`. |
| `raumfeld_speaker_udn` | yes | UDN of the speaker device that hosts the Raumfeld API. |
| `stream_host` | no | Host name or IPv4 address the speaker fetches the stream from (`http://<stream_host>:8000/stream.mp3`). Empty: the host's LAN IPv4 address (source address of the default route), detected at each start. Set it if the host has several networks or the detected address is wrong. |
| `start_gate_seconds` | no | 0-60, default 15. How long the song is held at its start while the speaker wakes; `0` turns it off. |
| `stuck_seconds` | no | 1-300, default 15. How long the speaker may hang while buffering before it is restarted. |
| `log_level` | yes | `debug`, `info` (default), `warning` or `error`, for the receiver and the adapter. |

## Use

1. Set the options and start the app. The log shows each process starting:
   `Starting MPD`, `Starting PoToken helper`, `Starting receiver as '<name>'`,
   `Starting Raumfeld adapter (stream http://<host>:8000/stream.mp3)`.
2. On the phone (same LAN), open YouTube Music, tap Cast and pick the name.
3. The adapter wakes the room and starts the stream; the song is held at its
   start (up to `start_gate_seconds`) until the speaker plays.

Each process is restarted by the app's supervisor if it exits; the log says
which one and with which exit code.

## Troubleshooting

- **Not in the cast menu:** phone and Home Assistant must be on the same LAN
  segment (multicast). Check the log for `SSDP listening`. Another device with
  the same name or another process on port `8008` also prevents it.
- **Speaker stays silent:** check the adapter's log lines. The speaker must
  reach `http://<stream_host>:8000/stream.mp3`; set `stream_host` if the
  detected address is wrong.
- **Stream stops after about a minute:** the PoToken helper is not running;
  look for its exit in the log.
