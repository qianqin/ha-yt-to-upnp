# DevOps

There is no runtime deployment target yet. Packaging as a Home Assistant app
comes later; its container image will also be built locally.

There is no hosted pipeline. Every check runs locally: natively on macOS
(Apple Silicon) and in a temporary Linux container for platform coverage.
The toolchain is pinned in `rust-toolchain.toml`; keep the Docker image tag
below in sync with its `channel`.

## Release

A release is a push to `master`. Push only after **all** local checks below
passed on the exact commit you are about to push (clean working tree,
`git status` shows nothing to commit). Run everything from the repository root.

### Local checks

1. macOS, native toolchain (picked up from `rust-toolchain.toml` via rustup):

   ```sh
   cargo build --locked && cargo test --locked
   ```

2. Linux, in a temporary container. The `target` directory lives in a
   throwaway Docker volume so Linux build artifacts do not mix with the macOS
   ones in `./target`:

   ```sh
   docker run --rm -v "$PWD":/src -w /src \
     -v ytcast-linux-target:/src/target \
     rust:1.99.1-bookworm bash -c \
     'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo test --locked'
   ```

   Optional second Linux variant on a plain Ubuntu base with rustup:

   ```sh
   docker run --rm -v "$PWD":/src -w /src \
     -v ytcast-ubuntu-target:/src/target \
     ubuntu:24.04 bash -c \
     'apt-get update -qq && apt-get install -y -qq curl build-essential pkg-config protobuf-compiler >/dev/null \
      && curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile minimal \
      && . "$HOME/.cargo/env" && cargo test --locked'
   ```

3. Dependency audit (fails on vulnerabilities; unmaintained/unsound advisories
   are reported as warnings). One-time install:

   ```sh
   cargo install cargo-audit --version 0.22.2 --locked
   ```

   Then:

   ```sh
   cargo audit
   ```

4. Raumfeld adapter tests (stdlib-only Python):

   ```sh
   python3 -m unittest discover -s adapter
   ```

5. Clean up the temporary Linux environment:

   ```sh
   docker volume rm ytcast-linux-target ytcast-ubuntu-target
   docker image rm rust:1.99.1-bookworm ubuntu:24.04
   ```

   (`docker volume rm` / `docker image rm` report an error for a volume or
   image the optional variant did not create; that is harmless.)

Then push:

```sh
git push origin master
```

### Check

There is no pipeline to watch. The release is **healthy** when the push
succeeded and the remote `master` points at the commit you checked locally:

```sh
git rev-parse HEAD
git ls-remote origin master
```

Both must print the same SHA.

### Rollback

Never force-push `master`. Revert the bad commit(s) and push the revert:

```sh
git revert --no-edit <sha>                    # single commit
git revert --no-edit <oldest-bad>^..<newest-bad>   # range
```

Run all local checks above on the reverted tree, then `git push origin master`
and repeat the health check.

## Targets

### Target: Test

Local build and test on the exact commit, natively on macOS and in a
temporary Linux container: steps 1, 2, and 4 of [Local checks](#local-checks),
followed by the cleanup in step 5.

### Target: Production

The push to `master` succeeded, `git ls-remote origin master` matches the
local commit, and all local checks (macOS build and tests, Linux container
tests, `cargo audit`, adapter tests) passed on that commit. There is no
pipeline to watch.
