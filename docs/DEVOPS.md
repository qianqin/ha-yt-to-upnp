# DevOps

There is no runtime deployment target yet. Packaging as a Home Assistant app
comes later; until then the only automated pipeline is CI.

## Release

A release is a push to `master`. The push triggers the CI workflow
(`.github/workflows/ci.yml`), which runs on the toolchain pinned in
`rust-toolchain.toml`:

1. `cargo build --locked`
2. `cargo test --locked`
3. `cargo audit` (fails on vulnerabilities; unmaintained/unsound advisories are warnings)

A release is **healthy** when the CI run for the pushed commit is green.

### Check

```sh
gh run list --branch master --workflow CI --limit 5
gh run watch <run-id> --exit-status
```

Make sure the run's `headSha` matches the commit you pushed
(`gh run list --branch master --json headSha,status,conclusion`).

### Rollback

Never force-push `master`. Revert the bad commit and push the revert; CI runs
again on the revert commit and must be green:

```sh
git revert <sha>
git push
```

## Targets

### Target: Test

Local build and test in the same pinned toolchain, via Docker (no local Rust
install needed). Run from the repository root:

```sh
docker run --rm -v "$PWD":/src -w /src \
  -v ytcast-cargo-registry:/usr/local/cargo/registry \
  -v ytcast-target:/src/target \
  rust:1.99.1-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo build --locked && cargo test --locked'
```

Keep the image tag in sync with `channel` in `rust-toolchain.toml`.

### Target: Production

CI on `master` is green for the pushed commit (build, tests, `cargo audit`).
