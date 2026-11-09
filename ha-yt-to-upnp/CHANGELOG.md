# Changelog

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
