# DevOps

The runtime deployment target is a Home Assistant **app installed from this
repository**: Home Assistant builds a thin image from `ha-yt-to-upnp/` on its
host, with programs prebuilt on the release machine and published as a GitHub
release asset; see [Home Assistant app](#home-assistant-app) below. No image
registry, no GitHub Actions, no compile on the Home Assistant host.

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

A release is a push to `master`. A new version of the Home Assistant app
also needs its GitHub release, published before that push: see
[App release](#app-release). Commit everything (clean working tree), then
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
tests). There is no pipeline to watch. For a new app version also: the
GitHub release `v<version>` is published, its asset has the sha256 in
`artefacts.lock` at that commit, and Home Assistant offers the update (see
[App release](#app-release), Check).

## Home Assistant app

The app folder `ha-yt-to-upnp/` (Home Assistant app layout: `config.yaml`,
`Dockerfile`, s6-overlay services under `rootfs/`, `DOCS.md`) is what Home
Assistant installs once the repository is added (`repository.yaml` at the
root names it). The Supervisor builds the image on the Home Assistant host
from that folder. The build compiles nothing: on the pinned base image it
installs Debian packages (`mpd`, `python3`) with apt and copies the prebuilt
programs, which its first stage (`fetch-artefacts.sh`) takes from:

- `artefacts/` in the build context, if there is one: a folder assembled with
  `scripts/build-ha-app.sh` (local app, smoke tests);
- otherwise the release tarball named in `ha-yt-to-upnp/artefacts.lock`
  (`version`, `url`, `sha256`), downloaded with `curl` over https only
  (redirects too) from
  `https://github.com/<owner>/<repo>/releases/download/v<version>/ha-yt-to-upnp-artefacts-<version>-amd64.tar.gz`.
  It fails closed: a sha256 other than the lock's, a lock for another
  version than `config.yaml`'s (copied into the stage; a `BUILD_VERSION`
  from the Supervisor must agree with it too), a failed download, more than
  512 MiB (with or without a Content-Length) or a missing entry stops the
  build. Files are extracted without
  their owners or stored permissions, and no setuid or setgid bit is kept.
  No secret is needed (public release). The build argument `ARTEFACTS_URL`
  is for `scripts/smoke-ha-app.sh --release-path` only: it may name
  `https://` or plain http to `host.docker.internal` or `127.0.0.1` with a
  port, nothing else (no user info, no `[]{}` or backslash, one line,
  checked as a whole), and a
  `build.yaml` must never set it. `scripts/fetch_artefacts_test.py` tests
  all of this on the host against a local http server (the smoke test runs
  it first); on the Mac `sh` is bash and `tar` is bsdtar, so for dash and
  GNU tar as in the build also run it as root in the smoke test's cached
  base image (`docker run --rm --platform linux/amd64 -v "$PWD":/src:ro
  --entrypoint python3 ytcast-ha-smoke-base:<hash> -I
  /src/scripts/fetch_artefacts_test.py`).

`ha-yt-to-upnp/artefacts/` is gitignored, so the repository's app folder never
holds programs; `scripts/release-check.sh` and `scripts/publish-release.sh`
stop if it is tracked anyway. There are no GitHub Actions and no image
registry.

**What the installation trusts.** Home Assistant builds whatever `master`
says: the `Dockerfile`, the run scripts and `artefacts.lock`, whose sha256
decides which tarball is accepted. The sha256 only guards the release asset
(a replaced or corrupted asset fails the build); it does not guard the
repository: whoever can push to `master` (or change the lock there) decides
what every installation runs at its next update. The integrity of an
installation therefore rests on the `master` commit and on the GitHub
account that can push it. Recommended, the owner decides: two-factor
authentication on that account, branch protection on `master` (no force
pushes, no deletion), immutable releases (GitHub's repository setting, so a
published asset cannot be replaced), and optionally signed commits.

Target: Home Assistant OS on x86-64 (`amd64`), host networking (SSDP/DIAL
multicast and the Raumfeld's UPnP).

### Build the folder

From the repository root; `ha-yt-to-upnp/` must be committed (the script
refuses uncommitted or untracked changes there and takes the folder from
`HEAD` with `git archive`):

```sh
scripts/build-ha-app.sh                      # receiver and adapter from HEAD
scripts/build-ha-app.sh --receiver-ref <ref> --adapter-ref <ref> --out <dir>
scripts/build-ha-app.sh --release            # also the release tarball and artefacts.lock
```

By default the receiver and the adapter are built from `HEAD`, the same commit
as the app folder, so the run scripts and the programs they start (and the
environment they pass, e.g. `RAUMFELD_HOST`, `ADAPTER_STATE_DIR`) are in step.
Only uncommitted changes in `ha-yt-to-upnp/` are refused; the receiver and the
adapter are taken from the commit too (`git archive`, `git show`), so commit
them before building. Pass other refs only to rebuild an older version
locally; `--release` refuses them.

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

With `--release` it also packs `artefacts/` (as `artefacts/` at the tarball's
top) into `dist/release/ha-yt-to-upnp-artefacts-<version>-amd64.tar.gz`, for
the `version` in `ha-yt-to-upnp/config.yaml` at `HEAD`
(`scripts/pack_artefacts.py`, tested by `python3 -I
scripts/pack_artefacts_test.py`). The tarball is reproducible: sorted
entries, every mtime the commit's time, owner `0:0` without names, modes
`0755`/`0644`, a gzip header without time or name; links must stay inside
the folder, also when followed through other links (realpath). It then
writes `ha-yt-to-upnp/artefacts.lock` (version, asset URL, sha256; also
into the assembled folder). Owner and repository of the
URL come from `git remote get-url origin` (a GitHub URL without credentials;
anything else stops the script before the build).

To move to another Rust release, update `RUST_CHANNEL` and `RUST_IMAGE`
(`docker buildx imagetools inspect rust:<channel>-bookworm` shows the index
digest) together with the receiver's `rust-toolchain.toml`.

### Smoke test

```sh
scripts/smoke-ha-app.sh                      # tests dist/ha-yt-to-upnp
scripts/smoke-ha-app.sh --quick              # seconds, before a live test
scripts/smoke-ha-app.sh --release-path       # Home Assistant's build from the repository
scripts/smoke-ha-app.sh --clean-cache        # removes --quick's cached base image
```

The full run is the one reviews and releases need (its build must log that
the artefacts stage took `artefacts/` from the build context); `--quick` (below) is a
check before a live test; `--release-path` (below) is needed for a release,
after the full run.

It first runs `scripts/ha_options_test.py` on the host: the option check the
app runs at each start (`rootfs/usr/lib/ytcast/check_options.py`) uses
exactly the schema in `config.yaml` and accepts and rejects the right values;
`scripts/ha_mpd_conf_test.py`: MPD's configuration gets the encoder in place
of the template's one `@STREAM_ENCODER@` (none or two stop the app);
`scripts/fetch_artefacts_test.py` (see the artefacts stage above).
Then it builds the image from the assembled folder for `linux/amd64` the way
the Supervisor does (the folder's `Dockerfile`, `BUILD_VERSION` from
`config.yaml`, `BUILD_ARCH=amd64`) in a buildx builder of its own, and runs it
on a temporary Docker bridge network: **no host networking and no published
ports**, so nothing is announced on the LAN. The bridge network does reach the
internet through Docker's NAT, and the smoke test needs that: for the image
build (Debian packages) and for the PoToken check below.

Run 1, with test options (placeholder UDNs, cast name `Smoke Test`, no
`raumfeld_host`, `stream_format` `flac`), checks that
MPD answers on `127.0.0.1:6600` (and not on the container's address); while
MPD plays a generated second of silence (MPD opens its httpd output only when
it plays) the stream answers on `http://<container>:8000/stream.flac` as
`audio/flac`, starting with `fLaC` and a STREAMINFO block of 48000 Hz, 2
channels, 16 bits, and the adapter gets that URL with `STREAM_MIME=audio/flac`; the receiver
serves its DIAL device description on 8008 with the configured name, and its
pre-roll silence on `http://localhost:8008/silence` (`200`, `audio/L16`, zero
samples, fetched with `curl` in the container) but answers `403` to the same
request on the container's address; `/data/adapter`, `/data/ytcast`,
`/data/mpd`, `/data/mpd/music` and `/data/mpd/playlists` are directories
with mode `0700`, owned by their service's user; the
PoToken helper answers `/ping` on `127.0.0.1:4416` only and mints a token on
`POST /get_pot` for a placeholder content binding (this fetches BotGuard from
YouTube/Google through the helper's restricted Deno permissions); every
service runs as its own non-root user, does not get the (dummy)
`SUPERVISOR_TOKEN` the container is started with and cannot read it from
`/run/s6/container_environment`; the options arrive as the expected
environment (auto-detected `stream_host`, `ADAPTER_STATE_DIR=/data/adapter`,
no `RAUMFELD_HOST`); the adapter logs that it finds no speaker
(`DeviceNotFound`) and keeps running; no service restarts within 20 s. Run 2
sets `raumfeld_host` to the unreachable `192.0.2.20` without
`raumfeld_speaker_udn`, an explicit `stream_host`, no optional timings and
`stream_format` `mp3`: the stream is `/stream.mp3`, `audio/mpeg`, its first
frame MPEG-1 Layer III at 320 kbit/s, 48 kHz, stereo; the adapter gets
`RAUMFELD_HOST`, `STREAM_MIME=audio/mpeg` and no `RAUMFELD_SPEAKER_UDN`, logs
`Raumfeld room not found yet` and keeps running with no service restart within
20 s.
Runs 3-5 check that an invalid `cast_name`, an invalid `raumfeld_host`
(`192.0.2.20:47365`) and neither `raumfeld_speaker_udn` nor `raumfeld_host`
each stop the app with one `FATAL` line naming the option, before any service
starts. Run 6 puts a symbolic link (to `/tmp`) at `/data/adapter`: the app
stops with one `FATAL` line naming it, and the link's target is left alone.
Run 7 checks that an invalid `stream_format` (`flac24`) stops the app with one
`FATAL` line naming the option and its values. The checks run in the
container through `docker exec --privileged` (reading other users' `/proc`
entries needs `CAP_SYS_PTRACE`); the app itself runs unprivileged. Then it
removes the containers, network, image, builder (with its cache) and the
buildkit image if it pulled it.

**`--quick`** builds and checks in seconds instead of minutes: one container
with `stream_format` `flac` and `raumfeld_host` `192.0.2.20` (all four
services up as their own users without `SUPERVISOR_TOKEN`, MPD answers, the
FLAC stream as in run 1, the DIAL description has the cast name, the PoToken
helper answers `/ping` (no token is minted), the adapter's environment) and,
in parallel, run 7. It polls every 0.5 s and has no stability wait. It builds
with the classic builder (`DOCKER_BUILDKIT=0`, which leaves no build cache)
on a cached base image: the `Dockerfile` up to its
`# smoke-quick-cache-boundary` line (base image, Debian packages, Deno and
bgutil), tagged `ytcast-ha-smoke-base:<hash>` (hash of that part of the
`Dockerfile` and the files it copies, labelled `io.github.ytcast.smoke-base`).
The first quick run, or one after Deno or bgutil changed, builds it (about
20 s plus the package download) and replaces an older one; later runs build
only the steps after the boundary, from a build context without Deno and
bgutil. The cached base image is the one thing a quick run leaves behind;
`--clean-cache` removes it with its untagged parent layers. With the
containerd image store, removing an image does not remove the untagged
images the classic builder made for each step, so both modes remove them
too (child first, stopping at a tagged image or one that is still used).
A quick run builds only the `Dockerfile`'s last stage, with its
`COPY --from=artefacts /artefacts/...` lines turned into copies from
`artefacts/` in the build context; the full run builds both stages, so the
artefacts stage takes the folder's `artefacts/`.

**`--release-path [TARBALL]`** tests the build Home Assistant makes from the
repository. It builds `ha-yt-to-upnp/` as committed (it refuses changed,
untracked or ignored files there, such as an `artefacts/` folder) with
BuildKit in a builder of its own, so the artefacts stage downloads the
release tarball (default `dist/release/<asset of config.yaml's version>`) and
checks it against `artefacts.lock`; the tarball must be the lock's. It serves
the tarball with `python3 -m http.server` on `127.0.0.1` of this machine,
which the build reaches as `host.docker.internal` (Docker Desktop) through
the build argument `ARTEFACTS_URL`: set only by this test, it replaces the
lock's URL (and allows plain http to this machine), never its sha256.
First, building the artefacts stage alone must fail: with `sha256 mismatch`
for a tampered copy of the tarball (a gzip member appended: still a valid
tarball); with a version error for `BUILD_VERSION` `0.0.0`; for a stale lock
(a copy of the folder whose lock says another version, no `BUILD_VERSION`);
for a lock naming the genuine tarball over plain http (no override: https
only); and for an `ARTEFACTS_URL` to another host. Then the full image is built from
the genuine tarball and gets the quick run's checks, plus: `/opt/BUILD-INFO`
is the tarball's, and the receiver, the adapter and `deno` in the image have
the SHA-256 that `BUILD-INFO` records. The server, builder (with its cache),
containers, network and image are removed at the end.

Run `shellcheck` on the scripts if it is installed:

```sh
shellcheck scripts/*.sh ha-yt-to-upnp/fetch-artefacts.sh
shellcheck -x -s bash ha-yt-to-upnp/rootfs/usr/lib/ytcast/common.sh \
  ha-yt-to-upnp/rootfs/etc/s6-overlay/scripts/*.sh \
  ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/*/run ha-yt-to-upnp/rootfs/etc/s6-overlay/s6-rc.d/*/finish
```

### Live test on a Mac

The app's image cannot be live-tested in Docker Desktop for Mac: even
`--network host` is the network of Docker's Linux VM (`192.168.65.x`), not
the Mac's LAN, so the phone's SSDP search and the adapter's search for the
Raumfeld zone renderer never reach the container, and the receiver would
announce an address nobody on the LAN can reach. `scripts/run-ha-app-local.sh`
runs the same four processes natively instead, configured as the app is:

```sh
scripts/run-ha-app-local.sh --name "<cast name>" --room-udn <room UDN> \
  --raumfeld-host <speaker IPv4> [--speaker-udn <UDN>] [--stream-format flac|mp3] \
  [--stream-host <Mac's LAN IPv4>] [--log-level debug]
scripts/run-ha-app-local.sh --stop
```

It writes the options to a temporary `options.json` (defaults as in
`config.yaml`) and checks them with the app's `check_options.py`; renders
MPD's configuration from the app's `mpd.conf.template` with the app's
`stream_format` encoder (paths moved to its own directory); runs Homebrew's
`mpd`, the PoToken helper from the assembled folder (`artefacts/bgutil` with
its Deno cache, Homebrew's `deno`), the receiver built from this checkout for
macOS (`cargo build --release --locked`; it warns if that is not the app's
receiver commit) and the adapter from the assembled folder, each with only
its own environment. They run in the background; state and logs are in
`$TMPDIR/ytcast-local-<uid>`, and `--stop` stops them and removes that
directory. Needs `mpd`, `deno` and `jq` (Homebrew) and `cargo` on `PATH`. The
Mac's firewall must let the phone reach port 8008 and the speaker port 8000.

### Install from the repository

1. Stop any other instance of the receiver on the LAN (only one device may
   use the cast name).
2. Settings → Apps → **Repositories**: add
   `https://github.com/qianqin/ha-yt-to-upnp` (the URL in `repository.yaml`).
3. The app **YouTube to UPnP** appears in the app store. Install it: the
   Supervisor builds the image on the host (a few minutes: Debian packages
   and the release tarball's download, nothing compiled).
4. Configuration tab: set `cast_name`, `raumfeld_room_udn`, and
   `raumfeld_speaker_udn` or `raumfeld_host` (or both; see `DOCS.md`), save,
   start. Check the log for the four `Starting ...` lines.

Updates: once `master` has a higher `version` in `ha-yt-to-upnp/config.yaml`,
Home Assistant offers **Update** (Settings → Apps → App store → ⋮ → **Check
for updates** looks right away); it rebuilds the image the same way and
restarts the app, keeping the options and `/data`.

### App release

Each app version has one GitHub release `v<version>` with one asset,
`ha-yt-to-upnp-artefacts-<version>-amd64.tar.gz`. It is published **before**
the push to `master`: Home Assistant reads `master`, so a version pushed
first would be offered with no tarball to download (its build would fail
closed until the release exists). A published release is never replaced:
`scripts/publish-release.sh` stops if the tag or the release exists; a fix is
a new version.

From the repository root, on the commit that is to become `master` (fast
forward):

1. Bump `version` in `ha-yt-to-upnp/config.yaml`, add a `## <version>` section
   to `ha-yt-to-upnp/CHANGELOG.md` (it becomes the release notes), commit.
2. `scripts/release-check.sh` must exit 0 (see [Release](#release)).
3. Build the folder, the tarball and the lock (the Rust build takes a while):

   ```sh
   scripts/build-ha-app.sh --release
   ```

4. Commit the lock:

   ```sh
   git add ha-yt-to-upnp/artefacts.lock
   git commit -m "build(ha-app): artefacts.lock for <version>"
   ```

5. Smoke tests, both must pass:

   ```sh
   scripts/smoke-ha-app.sh                  # dist/ha-yt-to-upnp, local artefacts
   scripts/smoke-ha-app.sh --release-path   # ha-yt-to-upnp/ as committed, tarball download
   ```

6. Publish the release (needs `gh auth login` with write access to the
   repository; no other secret):

   ```sh
   scripts/publish-release.sh --dry-run     # local checks, prints the commands
   scripts/publish-release.sh
   ```

   It checks that the tree is clean and `ha-yt-to-upnp/artefacts` is not
   tracked; that `artefacts.lock` at `HEAD` is for the version in
   `config.yaml`, names the asset URL of the `origin` repository and has the
   sha256 of `dist/release/<asset>`; that the tarball belongs to `HEAD`: the
   commits its `artefacts/BUILD-INFO` names have the same receiver sources
   (`src/`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `build.rs`,
   `proto/`), `adapter/raumfeld_adapter.py` and app folder (all of
   `ha-yt-to-upnp/` but `artefacts.lock` and `*.md`) as `HEAD`; that the
   CHANGELOG has the version's section; and that no local tag `v<version>`
   exists. `--dry-run` stops there and prints the commands. The real run
   then requires branch `master`, fetches origin's `master` and requires it
   to be an ancestor of `HEAD`, and requires that neither the tag on origin
   (`git ls-remote` exit 2; any other failure stops it) nor the release
   (`release not found`; any other answer stops it) exists. If origin's
   `master` already contains `HEAD`, it warns (Home Assistant is already
   offering the version) and still publishes, so that the asset appears as
   soon as possible. Then it pushes
   the tag `v<version>` (`HEAD`, no `--force`), creates a **draft** release
   from it with the tarball and the CHANGELOG section, downloads the draft's
   asset and checks its sha256 (a mismatch stops it before publishing),
   publishes the draft (`--latest`), and downloads the asset from its public
   URL (the lock's) and checks it again, trying up to 6 times 10 s apart
   while GitHub does not serve it yet; if that keeps failing, the release is
   published and only the check needs repeating (the message says how).
   `scripts/publish_release_test.py` tests all of this with fake `gh`,
   `curl` and `sleep` and a `git` that keeps everything off origin. By hand,
   the same is:

   ```sh
   git fetch origin master && git merge-base --is-ancestor origin/master HEAD
   git ls-remote --exit-code --tags origin refs/tags/v<version>  # must find nothing (exit 2)
   git push origin HEAD:refs/tags/v<version>
   gh release create v<version> dist/release/ha-yt-to-upnp-artefacts-<version>-amd64.tar.gz \
     --verify-tag --draft --title <version> --notes-file <CHANGELOG section>
   gh release download v<version> --pattern '*.tar.gz' --dir <tmp>   # shasum -a 256: the lock's
   gh release edit v<version> --draft=false --latest
   curl -fsSL --proto '=https' -o <tmp>/asset "$(sed -n 's/^url: //p' ha-yt-to-upnp/artefacts.lock)"
   shasum -a 256 <tmp>/asset                                         # the lock's sha256
   ```

   If it stops after pushing the tag but before publishing, delete the draft
   and the tag (`gh release delete v<version> --cleanup-tag`) only if the
   release was never published; otherwise bump the version.

7. Push `master`:

   ```sh
   git push origin master
   ```

#### Check

```sh
git ls-remote origin master                 # the local HEAD
gh release view v<version>                  # published, one asset
```

The publish script has already compared the public asset with the lock. In
Home Assistant: App store → ⋮ → **Check for updates**, then **Update** on the
app's page; the log shows `fetch-artefacts: sha256 ... matches
artefacts.lock` in the build output (Supervisor log) and the four
`Starting ...` lines.

#### Rollback

Home Assistant only offers versions upward, so a rollback is a new version
above the broken one; the broken release stays published (installs may use
it, and its lock commit names it).

- Usual way: revert the broken commits (`git revert`), bump `version`, add a
  CHANGELOG entry naming the version it returns to, and do the whole
  [App release](#app-release) from that commit: the programs are rebuilt from
  the reverted source.
- Without a rebuild, when the last good release's programs fit the reverted
  app folder: revert, bump `version`, and write `artefacts.lock` by hand as
  the last good one (`git show v<good>:ha-yt-to-upnp/artefacts.lock`) with
  only `version:` changed to the new version (url and sha256 stay those of
  `v<good>`). No new GitHub release then: smoke-test with
  `gh release download v<good> --pattern '*.tar.gz' --dir <tmp>` and
  `scripts/smoke-ha-app.sh --release-path <tmp>/<asset>` (a folder built with
  the last good `BUILD-INFO`'s refs serves the full run), and push `master`.
  `scripts/publish-release.sh` is not used (its URL check refuses this lock).

In Home Assistant, **Update** to the new version.

### Local app (without the repository)

A folder assembled with `scripts/build-ha-app.sh` carries `artefacts/` and
builds without any download, as a **local app**. Local apps live in
`/local_apps` on the Home Assistant host (current Home Assistant OS; not
`/addons`). Over SSH (the Terminal & SSH app maps `/local_apps`);
`<ha-host>` is the Home Assistant host:

```sh
tar -C dist -cf - ha-yt-to-upnp | ssh root@<ha-host> \
  'rm -rf /local_apps/ha-yt-to-upnp && tar -xf - -C /local_apps'
```

The Samba share app works as well: replace the folder
`local_apps/ha-yt-to-upnp` as a whole. Then App store → ⋮ → **Check for
updates**: the app appears under **Local apps** (slug `local_yt_to_upnp`),
next to the repository's (they use the same ports: run only one). Install
and configure it as above. To update it, copy a newly built folder with a
higher `version` and use **Update** (without a version bump, **Rebuild** on
the app's page).

### Open follow-ups

- MPD's control port has no password (see the app's `DOCS.md`, Known
  limitations). Adding one needs password support in the receiver (`MPD_HOST`
  / `MPD_PASSWORD`) and in the adapter; then the app can generate a random
  password under `/data` at first start and set `password` and
  `default_permissions ""` in MPD's configuration.
- The receiver writes `cast_name` unescaped into its DIAL device description;
  the schema forbids `<`, `>`, `&` and `"` until it escapes the name.
