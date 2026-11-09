# YouTube to UPnP

Makes a Teufel Raumfeld room appear in the **YouTube Music** cast menu. The
app bundles four processes:

| Process | What it does | Listens on |
| --- | --- | --- |
| Receiver (`ytcast-lite`) | Cast target for YouTube Music (DIAL + YouTube Lounge API), streams the audio into MPD; plays silence into MPD while a cast loads (`/silence`, answered on loopback only) | `8008/tcp` (DIAL, LAN), `1900/udp` (SSDP multicast, LAN) |
| MPD | Plays the audio and re-serves it as an MP3 stream (192 kbit/s) | `8000/tcp` (stream, LAN), `127.0.0.1:6600` (control, loopback only) |
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
and decoder cache (`/data/ytcast`), MPD's state (`/data/mpd`, the volume) and
where the adapter last found the room's zone renderer (`/data/adapter`, no
secrets), so a restart needs no search for it.
It never changes the speaker's volume: the volume from the phone is MPD's
software volume.

## Before you start

- **Only one receiver with the same cast name on the LAN.** Stop any other
  instance (a test setup on another computer, for example) before starting the
  app; two devices with the same name confuse the phone.
- Find the UDNs of the Raumfeld room and of the speaker device that hosts the
  Raumfeld API (UPnP device descriptions or the Raumfeld host API's
  `getZones`). UDNs look like `uuid:00000000-0000-0000-0000-000000000001`.
- Optional, for a faster start: the LAN IPv4 address of that speaker device
  (the **Raumfeld host**), see [Finding the Raumfeld host](#finding-the-raumfeld-host).

## Options

| Option | Required | Meaning |
| --- | --- | --- |
| `cast_name` | yes | Name in the cast menu. Up to 60 characters, no `<`, `>`, `&` or `"`. |
| `raumfeld_room_udn` | yes | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001`. |
| `raumfeld_speaker_udn` | without `raumfeld_host` | UDN of the speaker device that hosts the Raumfeld API; the adapter finds it on the LAN by an SSDP search for this UDN. Not needed with `raumfeld_host` (checked if set). |
| `raumfeld_host` | no | IPv4 address (recommended) or DNS name of the Raumfeld host, without port, `http://` or path, e.g. `192.0.2.20`. The adapter reaches its API directly instead of searching the LAN for it, so a cast starts sooner. It must be the address the host answers SSDP from (its address on the speakers' LAN): the room's zone renderer is still found by a multicast search, and only answers from this address count. A host that doesn't list `raumfeld_room_udn` is an error, never a wrong device. A DNS name is only as trustworthy as the LAN's name resolution (mDNS, the router's DNS); use the fixed IPv4 address where possible. Empty: found by `raumfeld_speaker_udn`. |
| `stream_host` | no | Host name or IPv4 address the speaker fetches the stream from (`http://<stream_host>:8000/stream.mp3`). Empty: the host's LAN IPv4 address (source address of the default route), detected at each start. Set it if the host has several networks or the detected address is wrong. |
| `start_gate_seconds` | no | 0-60, default 15. How long the song is held at its start while the speaker wakes; `0` turns it off. |
| `stuck_seconds` | no | 1-300, default 15. How long the speaker may hang while buffering before it is restarted. |
| `log_level` | yes | `debug`, `info` (default), `warning` or `error`, for the receiver and the adapter. |

The app checks the options again at each start. If one is missing or invalid,
it starts nothing and stops, with one error in the log naming the option and
the rule it breaks (for example `option 'cast_name' is invalid: must match
...`, or `option 'raumfeld_speaker_udn' is required when raumfeld_host is not
set`). Fix the option and start the app again.

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
   `Starting Raumfeld adapter (stream http://<host>:8000/stream.mp3)`.
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
  reach `http://<stream_host>:8000/stream.mp3`; set `stream_host` if the
  detected address is wrong.
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
  playback, the queue and the volume. Neither the receiver nor the adapter can
  send an MPD password yet. Nothing outside the host can reach the port.
