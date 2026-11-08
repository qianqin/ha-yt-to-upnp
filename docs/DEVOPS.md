# DevOps

There is no runtime deployment target yet. Packaging as a Home Assistant app
comes later; its container image will also be built locally.

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
   docker image rm rust:1.99.1-bookworm
   ```

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

Local build and test on the exact commit, natively on macOS and in a
temporary Linux container, with the Docker resources removed afterwards:
`scripts/release-check.sh` (steps 1, 2, and 4 of the local checks, plus
cleanup).

### Target: Production

The push to `master` succeeded, `git ls-remote origin master` matches the
local commit, and `scripts/release-check.sh` exited 0 on that commit (macOS
build and tests, Linux container build and tests, `cargo audit`, adapter
tests). There is no pipeline to watch.
