# Releasing the Runner

For whoever cuts the release. Somebody installing the product wants
[AlgoJudge-Ops](https://github.com/AlgoJudge/AlgoJudge-Ops) instead.

## Where the version lives

**`Cargo.toml`, `[workspace.package]`, one line.** All six crates inherit it
with `version.workspace = true`, so a release changes that line and
`Cargo.lock` follows — check the lock in, do not hand-edit it.

## What a tag does

`.github/workflows/release.yml` runs on a pushed tag matching `v*` and refuses
one that does not point at a commit on `main`, or a name that is not
`v<major>.<minor>.<patch>[-prerelease]`.

**It publishes five images under one version**, because a Runner without its
language images judges nothing:

`algojudge-runner`, `lang-gcc`, `lang-clang`, `lang-python`, `lang-pypy`.

For `v0.1.0` each gets `0.1.0`, `0.1`, `0` and `latest`. **A prerelease
publishes its own tag alone.** They are built, checked and pushed as one set, or
not at all — a release is tested as a set, and a deployment is meant to pin the
same version across all five.

`linux/amd64` only. That is not an oversight: the measurement rests on cgroup v2
on amd64, and a submission's container has to match the architecture of the host
running it.

## Before the tag

- [ ] `Cargo.toml` says the version being released, and `Cargo.lock` agrees.
- [ ] `README.md` names that version in the five `docker pull` lines and the four
      `AJ_Sandbox__Image__*` lines.
- [ ] The commit is on `main`, and **its** CI run is green.
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo build --workspace --release`
- [ ] `cargo test --workspace` — this includes the test that compares
      `.env.example` against what the code actually reads, in both directions: a
      key added to the source and not to that file fails here, and so does a key
      listed there and read by nothing.
- [ ] The four language images build, and their toolchains answer: the `docker`
      job in `.github/workflows/ci.yml` is the list.
- [ ] The conformance suite has been run against a real daemon —
      `./x test --test conformance -- --include-ignored --test-threads=1`. It is
      what stands between a release and a sandbox that does not isolate.
- [ ] The Rust base image digest in `Dockerfile` and `Dockerfile.toolchain` is
      the one intended, and **the same digest the external Runner pins**.

## After the tag

The five images have to exist before an installation can pull them.
`AlgoJudge-Ops` asks for the moving major `0` and pulls all five by that tag.

The documentation site cuts its `/runner/` snapshot on release day.
