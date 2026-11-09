# DevOps

The runtime deployment target is a Home Assistant **local app**, built on the
release machine and copied to the Home Assistant host; see
[Home Assistant app](#home-assistant-app) below. No image registry is used.

There is no hosted pipeline. Every check runs locally: natively on macOS
(Apple Silicon) and in a temporary Linux container for platform coverage.
The toolchain is pinned in `rust-toolchain.toml`; the Linux image tag
(`rust:<channel>-bookworm`) follows its `channel`.

## Prerequisites

One-time setup on the release machine:

- **rustup** with its `cargo` on `PATH`. rustup picks up the pinned toolchain
  from `rust-toolchain.toml` automatically (e.g. `brew install rustup`, then
  put `$(brew --prefix rustup)/bin` on `PATH`).
- **protoc**, needed by `build.rs`: `brew install protobuf`
- **Docker** (running daemon), for the Linux checks.
- **python3**, for the adapter tests.
- **cargo-audit 0.22.2**:

  ```sh
  cargo install cargo-audit --version 0.22.2 --locked
  ```

## Release

A release is a push to `master`. Commit everything (clean working tree), then
run from the repository root:

```sh
scripts/release-check.sh
```

**Push only if it exits 0:**

```sh
git push origin master
```

The script refuses to run on a dirty tree, runs the local checks below in
order on the current commit, stops at the first failure, and always removes
the Docker volumes and containers it created plus any image it had to pull
(images that were already present are left alone). Add `--with-ubuntu` to
also run the optional Ubuntu variant.

To abort during a container step, press Ctrl-C up to three times: the
container keeps building until the `docker` CLI exits. Cleanup still runs.

### Local checks (what the script runs)

For reference, the same checks by hand, from the repository root:

1. macOS, native toolchain:

   ```sh
   cargo build --locked && cargo test --locked
   ```

2. Linux, in a temporary container. The source is mounted read-only; build
   output goes to a throwaway named volume, so Linux artifacts never mix with
   the macOS ones in `./target`:

   ```sh
   docker run --rm -v "$PWD":/src:ro -w /src \
     -v ytcast-linux-target:/target -e CARGO_TARGET_DIR=/target \
     rust:1.99.1-bookworm bash -c \
     'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo build --locked && cargo test --locked'
   ```

   Optional variant on a plain Ubuntu base with rustup:

   ```sh
   docker run --rm -v "$PWD":/src:ro -w /src \
     -v ytcast-ubuntu-target:/target -e CARGO_TARGET_DIR=/target \
     ubuntu:24.04 bash -c \
     'apt-get update -qq && apt-get install -y -qq curl ca-certificates build-essential pkg-config protobuf-compiler >/dev/null \
      && curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none \
      && . "$HOME/.cargo/env" && cargo build --locked && cargo test --locked'
   ```

3. Dependency audit (fails on vulnerabilities; unmaintained/unsound advisories
   are reported as warnings):

   ```sh
   cargo audit
   ```

4. Raumfeld adapter tests (stdlib-only Python):

   ```sh
   python3 -m unittest discover -s adapter
   ```

5. Cleanup. Run this **every time**, also when a step above failed. One
   command per resource:

   ```sh
   docker volume rm ytcast-linux-target
   ```

   Remove `rust:1.99.1-bookworm` (`docker image rm rust:1.99.1-bookworm`) only
   if you pulled it for this run; the cross-compile in the README uses the same
   image.

   Only if you ran the optional Ubuntu variant:

   ```sh
   docker volume rm ytcast-ubuntu-target
   ```

   Remove `ubuntu:24.04` (`docker image rm ubuntu:24.04`) only if you pulled it
   for this run and nothing else uses it.

### Check

There is no pipeline to watch. The release is **healthy** when the push
succeeded and the remote `master` points at the commit you checked locally:

```sh
git rev-parse HEAD
git ls-remote origin master
```

Both must print the same SHA.

### Rollback

Never force-push `master`. Revert the bad commit(s) and commit the revert:

```sh
git revert --no-edit <sha>                         # single commit
git revert --no-edit <oldest-bad>^..<newest-bad>   # range
```

Run `scripts/release-check.sh` on the reverted tree, push only if it exits 0
(`git push origin master`), and repeat the health check.

## Targets

### Target: Test

All local checks on the exact commit, with the Docker resources removed
afterwards: `scripts/release-check.sh` (steps 1 to 4 of the local checks:
macOS and Linux container build and tests, `cargo audit`, adapter tests,
plus cleanup).

### Target: Production

The push to `master` succeeded, `git ls-remote origin master` matches the
local commit, and `scripts/release-check.sh` exited 0 on that commit (macOS
build and tests, Linux container build and tests, `cargo audit`, adapter
tests). There is no pipeline to watch.

## Home Assistant app

The app folder `ha-yt-to-upnp/` (Home Assistant app layout: `config.yaml`,
`Dockerfile`, s6-overlay services under `rootfs/`, `DOCS.md`) is completed by
prebuilt artefacts and installed as a **local app**: the Supervisor builds its
image on the Home Assistant host from the copied folder. That build only
installs Debian packages (`mpd`, `python3`) on the pinned base image and copies
files; nothing is compiled or fetched from GitHub on the host. There are no
GitHub Actions and no image registry.

Target: Home Assistant OS on x86-64 (`amd64`), host networking (SSDP/DIAL
multicast and the Raumfeld's UPnP).

### Build the folder

From the repository root; `ha-yt-to-upnp/` must be committed (the script
refuses uncommitted or untracked changes there and takes the folder from
`HEAD` with `git archive`):

```sh
scripts/build-ha-app.sh                      # receiver 7a699f5, adapter 556d421
scripts/build-ha-app.sh --receiver-ref <ref> --adapter-ref <ref> --out <dir>
```

The result is `dist/ha-yt-to-upnp/` (gitignored): the app folder files plus
`artefacts/`:

| Artefact | Built from | Verified by |
| --- | --- | --- |
| `ytcast-lite` | receiver at `--receiver-ref`, `git archive` into a temp dir, `cargo build --locked --release --target x86_64-unknown-linux-gnu` in a temporary `rust:1.99.1-bookworm` container pinned by digest (`--platform linux/amd64`; `protoc` from Debian bookworm installed in it, not pinned, its version recorded) | `Cargo.lock` (`--locked`), the receiver's `rust-toolchain.toml` channel must equal the image's (`RUST_CHANNEL`), ELF x86-64 check |
| `adapter/raumfeld_adapter.py` | `adapter/raumfeld_adapter.py` at `--adapter-ref` (stdlib only) | git commit |
| `deno` | Deno 2.9.8 release zip for `x86_64-unknown-linux-gnu` | SHA-256 pinned in the script |
| `bgutil/` | bgutil-ytdlp-pot-provider server at commit `26475e9` (tag 2.0.2), `npm ci --omit=dev --ignore-scripts` and `deno cache --frozen src/main.ts` in a temporary pinned `node` container (`--platform linux/amd64`), Deno cache under `bgutil/.deno` for the image path `/opt/bgutil` | commit hash, `package-lock.json` integrity hashes, `deno.lock`; no native build output (`*.node`, `node_modules/canvas/build`) |
| `BUILD-INFO` | `HEAD` commit of the app folder, resolved commits, build images with digests, `protoc` version and SHA-256 of every artefact | |

The script fails closed on any checksum or commit mismatch, refuses to replace
an `--out` folder that is not a previous build, and removes its temp dir,
containers, the target volume and every image it had to pull. The amd64 Rust
build runs emulated on Apple Silicon and takes a while.

To move to another Rust release, update `RUST_CHANNEL` and `RUST_IMAGE`
(`docker buildx imagetools inspect rust:<channel>-bookworm` shows the index
digest) together with the receiver's `rust-toolchain.toml`.

### Smoke test

```sh
scripts/smoke-ha-app.sh                      # tests dist/ha-yt-to-upnp
```

It first runs `scripts/ha_options_test.py` on the host: the option check the
app runs at each start (`rootfs/usr/lib/ytcast/check_options.py`) uses
exactly the schema in `config.yaml` and accepts and rejects the right values.
Then it builds the image from the assembled folder for `linux/amd64` the way
the Supervisor does (the folder's `Dockerfile`, `BUILD_VERSION` from
`config.yaml`, `BUILD_ARCH=amd64`) in a buildx builder of its own, and runs it
on a temporary Docker bridge network: **no host networking and no published
ports**, so nothing is announced on the LAN. The bridge network does reach the
internet through Docker's NAT, and the smoke test needs that: for the image
build (Debian packages) and for the PoToken check below.

With test options (placeholder UDNs, cast name `Smoke Test`) it checks that
MPD answers on `127.0.0.1:6600` (and not on the container's address); while
MPD plays a generated second of silence (MPD opens its httpd output only when
it plays) the stream answers on port 8000 as `audio/mpeg`; the receiver
serves its DIAL device description on 8008 with the configured name; the
PoToken helper answers `/ping` on `127.0.0.1:4416` only and mints a token on
`POST /get_pot` for a placeholder content binding (this fetches BotGuard from
YouTube/Google through the helper's restricted Deno permissions); every
service runs as its own non-root user, does not get the (dummy)
`SUPERVISOR_TOKEN` the container is started with and cannot read it from
`/run/s6/container_environment`; the options arrive as the expected
environment (auto-detected and explicit `stream_host`, unset optional
timings); the adapter logs that it finds no speaker and keeps running; no
service restarts within 20 s; and an invalid `cast_name` stops the app with
one error naming the option, before any service starts. The checks run in the
container through `docker exec --privileged` (reading other users' `/proc`
entries needs `CAP_SYS_PTRACE`); the app itself runs unprivileged. Then it
removes the containers, network, image, builder (with its cache) and the
buildkit image if it pulled it.

Run `shellcheck` on the scripts if it is installed:

```sh
shellcheck scripts/*.sh
shellcheck -x -s bash ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh \
  ha-yt-to-upnp/rootfs/etc/s6-overlay/scripts/*.sh \
  ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/*/run ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/*/finish
```

### Copy to the Home Assistant host

Over SSH (the Terminal & SSH app maps the `/addons` share); `<ha-host>` is the
Home Assistant host:

```sh
tar -C dist -cf - ha-yt-to-upnp | ssh root@<ha-host> \
  'rm -rf /addons/ha-yt-to-upnp && tar -xf - -C /addons'
```

The Samba share app works as well: replace the folder `addons/ha-yt-to-upnp`
as a whole.

### Install

1. Stop any other instance of the receiver on the LAN (only one device may
   use the cast name).
2. Settings → Apps → App store → ⋮ → **Check for updates**. The app appears
   under **Local apps** (slug `local_yt_to_upnp`).
3. Install it. The Supervisor builds the image on the host (a few minutes:
   Debian packages).
4. Configuration tab: set `cast_name`, `raumfeld_room_udn`,
   `raumfeld_speaker_udn` (see `DOCS.md`), save, start. Check the log for the
   four `Starting ...` lines.

### Update

1. Bump `version` in `ha-yt-to-upnp/config.yaml`, add a `CHANGELOG.md` entry,
   commit.
2. Build the folder and run the smoke test (above).
3. Copy the folder to the host (above).
4. App store → ⋮ → **Check for updates**; the app offers **Update**, which
   rebuilds the image and restarts the app. Options and `/data` are kept.
   Without a version bump, use **Rebuild** on the app's page instead.

Rollback: revert the broken change in a new commit (`git revert`) that also
bumps `version` above the broken one and adds a `CHANGELOG.md` entry, build
the folder from that commit with the receiver and adapter refs of the last
good `BUILD-INFO`, run the smoke test, copy it and update.

### Open follow-ups

- MPD's control port has no password (see the app's `DOCS.md`, Known
  limitations). Adding one needs password support in the receiver (`MPD_HOST`
  / `MPD_PASSWORD`) and in the adapter; then the app can generate a random
  password under `/data` at first start and set `password` and
  `default_permissions ""` in MPD's configuration.
- The receiver writes `cast_name` unescaped into its DIAL device description;
  the schema forbids `<`, `>`, `&` and `"` until it escapes the name.
