# Raumfeld adapter

A small sidecar (Python 3.9+, standard library only) that makes a Teufel
Raumfeld room play the receiver's MPD stream automatically.

- When MPD starts playing, it finds the speaker via SSDP (by UDN, never by IP),
  wakes the room (`leaveStandby`), reuses the room's existing zone (or connects
  it to a new one only if it has none) and tells the zone's renderer to play
  `STREAM_URL` via UPnP AVTransport.
- If the zone already plays the stream, it does nothing. If the zone is playing
  the Raumfeld **line-in** (e.g. another cast device), it does not take over.
- When MPD is stopped or paused for `IDLE_MINUTES`, it stops the zone and puts
  the room into manual standby, but only if the zone still holds our stream
  (playing, paused or stopped). Anything else, e.g. line-in, is left alone.
- It never changes the volume and sends no RenderingControl requests at all.
- Devices are untrusted: it only fetches plain `http://` URLs on the host that
  answered SSDP (redirects included), caps replies at 256 KiB and each request
  at 10 s in total (connect, headers, body), refuses XML with DOCTYPE/ENTITY,
  and accepts only `uuid:<letters, digits, dashes>` UDNs.
- Network and device errors are logged, never fatal. A failed takeover is
  retried on the next MPD event or after 30 seconds while MPD keeps playing; a
  failed release is retried after another `IDLE_MINUTES`. Lost MPD connections
  are re-established with backoff.

## Configuration (environment)

| Variable | Default | Meaning |
| --- | --- | --- |
| `RAUMFELD_ROOM_UDN` | required | UDN of the Raumfeld room, e.g. `uuid:00000000-0000-0000-0000-000000000001` |
| `RAUMFELD_SPEAKER_UDN` | required | UDN of the speaker device that hosts the Raumfeld API |
| `STREAM_URL` | required | MPD httpd stream (`http://` or `https://`), e.g. `http://<receiver-host>:8000/stream.mp3` |
| `STREAM_MIME` | `audio/mpeg` | MIME type announced in the DIDL-Lite metadata |
| `STREAM_METADATA` | `1` | `0` sends empty `CurrentURIMetaData` instead of DIDL-Lite |
| `IDLE_MINUTES` | `5` | How long MPD must be stopped/paused before the room is released |
| `MPD_HOST` / `MPD_PORT` | `localhost` / `6600` | MPD control connection |
| `LOG_LEVEL` | `INFO` | `DEBUG`, `INFO`, `WARNING`, `ERROR`, `CRITICAL` |

Missing or invalid settings make the adapter exit with status 2 and a message
naming the variable.

See [`mpd.conf.example`](mpd.conf.example) for a matching MPD setup (httpd
output on port 8000, MP3 or FLAC).

## Run

From the repository root:

```sh
RAUMFELD_ROOM_UDN=uuid:... RAUMFELD_SPEAKER_UDN=uuid:... \
STREAM_URL=http://<receiver-host>:8000/stream.mp3 \
python3 -m adapter
# or: python3 adapter/raumfeld_adapter.py
```

SSDP needs multicast on the LAN, so in a container use host networking.

## Tests

```sh
python3 -m unittest discover -s adapter -v
```

The tests use local fake servers (MPD, Raumfeld host API, device description,
AVTransport SOAP, SSDP responder) on 127.0.0.1 and need no real devices.
