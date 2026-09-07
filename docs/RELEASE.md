# Releasing the Runner

For whoever cuts the release. Somebody installing the product wants
[AlgoJudge-Ops](https://github.com/AlgoJudge/AlgoJudge-Ops) instead.

Every dated claim below was checked on **2026-09-07** against `release/0.1.0` at
`77a20fd`. Where something could not be checked, this file says so rather than
leaving it to be read as verified.

## Nothing has been released yet

`git tag -l` is empty and `ghcr.io/algojudge` holds no image of this repository.
So `0.1.0`, `0.1`, `0` and `latest` are all created by this tag, there is no
upgrade path to reason about, and nothing anywhere is pinned to a moving tag
that is about to start moving.

## Where the version lives

**`Cargo.toml`, `[workspace.package]`, one line.** All six crates inherit it
with `version.workspace = true` — checked in all six manifests — so a release
changes that line and `Cargo.lock` follows. Check the lock in, do not hand-edit
it. Both say `0.1.0` today.

## The tag does not go on this branch

`release/0.1.0` is **two commits ahead of `main` and none behind**, and both are
the version-naming commits. Two things follow:

- **CI runs on `main` alone.** `.github/workflows/ci.yml` triggers on `push` and
  `pull_request` against `main`, so neither of those two commits has a run of
  its own — `gh run list --branch release/0.1.0` is empty. The last green run on
  `main` is `34041266179`, 2026-09-06, at `3b46ea3`, the parent of both.
- **`release.yml` refuses a tag that is not an ancestor of `main`.** The branch
  is merged by pull request first, and the tag goes on the `main` commit that
  results, once *that* commit's own CI run is green.

## This Runner is released before the external Runner

`AlgoJudge-External-Runner` takes the `aj-protocol` crate from here, pinned in
its `Cargo.toml` to the **revision** `490d2e2` — a commit, not a branch and not
a moving tag. That revision is an ancestor of `release/0.1.0`, and
`crates/aj-protocol` has not changed since it, so the crate it compiles today is
the crate this release ships.

Its manifest gives the reason for a revision: *AlgoJudge-Runner publishes no
tags, so a revision is the only stable name.* `v0.1.0` is the first tag, so what
this release owes that repository is the name it has been doing without — the
commit `v0.1.0` points at — and the Rust base digest below, which its own
checklist requires to equal this one's.

**It names no image published here.** It compiles nothing and runs nothing: one
image of its own, `algojudge-external-runner`, and no language images. The
repository that names all five of ours is `AlgoJudge-Ops`, whose `RUNNER_TAG`
covers the Runner and its four language images together and defaults to the
moving major `0`.

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

## The toolchain and the base images

- `rust-version = "1.97"`. `Dockerfile`, `Dockerfile.toolchain` and the `build`
  job in `ci.yml` all pin
  `rust@sha256:3b2879047d42784ca9403ad20c51ed3df361a50f1df96f5777d39b4e33aa65cd`,
  which holds `rustc 1.97.1 (8bab26f4f 2026-07-14)` and `cargo 1.97.1` — read
  from the image itself.
- `AlgoJudge-External-Runner` pins the **same digest** in both of its files.
- **Whether a newer stable Rust exists was not checked**: nothing here queries
  the release channel, and a digest says what it is, never what it is behind.
- **The four language images pin nothing.** Their bases are `debian:trixie-slim`
  — gcc, clang, and the shim stage in all four — `python:3-slim` and
  `pypy:3-slim`, which are tags, so the same commit can build different
  compilers on different days while a problem author sets limits against one of
  them. What a local build of 2026-09-07 holds, all four on Debian 13 (trixie):
  **GCC and G++ 14.2.0**, **Clang 19.1.7**, **CPython 3.14.7**, **PyPy 3.11.15**.
  Record what the release actually shipped:
  `docker run --rm <image> cat /etc/algojudge-toolchain` prints it.

## The dependencies

Both figures below are from 2026-09-07 and both go stale.

- **No advisory against anything in `Cargo.lock`.** `cargo audit` 0.22.2 scanned
  252 crate dependencies against 1239 advisories and found none. One warning:
  **`chacha20` 0.10.1 is yanked**, reached through `rand` 0.10.2.
- **`cargo audit` is not in the toolchain image.** Install it into the `./x`
  cargo volume first: `./x install cargo-audit --locked`, then `./x audit`.
- **`Cargo.lock` is behind inside its own ranges.** `./x update --dry-run` moves
  58 entries, none of them an advisory fix. Eight more are behind their latest
  across a major and would need a range changed in `Cargo.toml`: `base64`
  0.22.1 to 0.23.1, `bollard` 0.19.4 to 0.21.1, `ed25519-dalek` 2.2.0 to 3.0.0,
  `generic-array` 0.14.7 to 0.14.9, `rand_core` 0.6.4 to 0.10.1, `reqwest`
  0.12.28 to 0.13.4, `sha2` 0.10.9 to 0.11.0, `zip` 2.4.2 to 8.6.0.

## How far `.env.example` is checked

`crates/aj-runner/src/config.rs`,
`every_variable_the_config_reads_is_in_the_example_and_no_others`, runs in
`cargo test --workspace` and was green on 2026-09-07. **It compares the key set
in both directions and nothing else**, and four limits are worth knowing before
a green run is trusted:

- **Not a value, a default, a comment or the ordering.** Those are read by a
  person or by nobody.
- **`AJ_`-prefixed keys only**, and only in nine sections — `Server`, `Runner`,
  `Cache`, `Lease`, `Poll`, `Heartbeat`, `Work`, `Sandbox`, `Pipes`. A key in a
  tenth is invisible in both directions.
- **Two places in the source only**: `config.rs` itself, and every `.rs` under
  `crates/aj-sandbox/src/`. A key read anywhere else is invisible.
- **The keys without the prefix are listed by hand and checked by nothing**:
  `DOCKER_HOST`, `RUST_LOG`, `NO_COLOR`, `HTTP_PROXY`, `HTTPS_PROXY`,
  `ALL_PROXY`, `NO_PROXY`, the two under *Do not set these*, and `HOSTNAME`,
  which is described where it is used rather than offered to be typed.

Read by hand on 2026-09-07 against `config.rs` and `cgroups.rs`: every default
the file states matches the source. The one defect the check cannot see is under
*Corrections* below.

## Before the tag

### The version

- [ ] `Cargo.toml` says the version being released, and `Cargo.lock` agrees.
- [ ] `README.md` names that version in the five `docker pull` lines and the four
      `AJ_Sandbox__Image__*` lines.
- [ ] `release/0.1.0` is merged into `main` by pull request, and the tag names a
      commit on `main` whose **own** CI run is green.

### The gate

`./x gate` is the `build` job of `ci.yml`, in the same order, against the same
pinned toolchain. Rust is not a prerequisite; `./x` runs cargo in a container.

- [ ] `./x gate` — `cargo fmt --all --check`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo build --workspace --release`,
      `cargo test --workspace`.
- [ ] That suite carries the `.env.example` comparison above. A green run is
      parity of the **key set**; read the comments and the sample values too.
- [ ] `./x install cargo-audit --locked && ./x audit`, and record the date. The
      figures under *The dependencies* describe 2026-09-07 and are not evidence
      about the day of the release.
- [ ] The Rust base digest in `Dockerfile` and `Dockerfile.toolchain` is the one
      intended, and **the same digest the external Runner pins**. Two Rust images
      a month apart is two compilers nobody chose.
- [ ] `.env.example` is the only `.env*` in the repository. `git ls-files` and
      the working tree both said so on 2026-09-07; `.gitignore` ignores `/.env`
      and `/.env.*` and re-admits the example alone, a pattern that has swallowed
      it before.

### The containers, against a real daemon

Everything here needs a container runtime, is `#[ignore]`d so that an ordinary
`./x test` stays fast, and needs `--test-threads=1` because these fight over the
daemon when run in parallel. All of them run in CI, on **both** cgroup drivers.

**The adversarial suite is what stands between a release and a sandbox that does
not isolate** — one case per thing untrusted code will try. The conformance
suite is the other half: the wire protocol, against a real Server.

- [ ] Build the four language images first. Nothing builds them for you, and
      every judging case fails at its first line without them:

      docker build -t algojudge/lang-gcc:local    -f images/gcc/Dockerfile images/
      docker build -t algojudge/lang-clang:local  -f images/clang/Dockerfile images/
      docker build -t algojudge/lang-python:local -f images/python/Dockerfile images/
      docker build -t algojudge/lang-pypy:local   -f images/pypy/Dockerfile images/

- [ ] `AJ_DOCKER_SOCKET=1 ./x test -p aj-sandbox --test adversarial --
      --include-ignored --test-threads=1`
- [ ] `AJ_DOCKER_SOCKET=1 ./x test -p aj-standard-io --test judging --
      --include-ignored --test-threads=1`
- [ ] With the development stack up —
      `docker compose -f example-runner-development-docker-compose.yaml up -d --build --wait` —
      and `AJ_DOCKER_NETWORK=algojudge-runner-dev_default`,
      `AJ_TEST_SERVER=http://server:8080/api/v1`:
      `./x test --test conformance -- --include-ignored --test-threads=1`, then
      `./x test -p aj-runner --test end_to_end -- --include-ignored
      --test-threads=1`. The end-to-end case
      `a_window_does_not_cost_a_participant_their_submission` needs
      `AJ_DOCKER_NETWORK=container:algojudge-runner-dev-server-1` and
      `AJ_TEST_SERVER=http://127.0.0.1:8080/api/v1` instead, because the
      maintenance switch answers only on the Server's own loopback interface.
- [ ] The image builds and the toolchains answer. `ci.yml`'s `docker` job builds
      the Runner image and nothing else; the four language images are built in
      its `integration` and `adversarial` jobs, and the step that runs each
      toolchain — `g++`, `gcc`, `clang`, `python3`, `pypy3` — is in
      `release.yml`, where it gates the push on the tag and nothing earlier.

### Corrections this release is waiting on

Each is a claim the tag would publish as it stands, found on 2026-09-07. None is
a defect in what the Runner does.

- [ ] `Cargo.toml:12` — the comment says `aj-standard-io` "arrives next". It is
      a workspace member at line 8.
- [ ] `.github/workflows/release.yml:10` and `:85` — "Three images, one version"
      and "Build all three". It builds and pushes five.
- [ ] `.github/workflows/ci.yml:275` — "publishes this image and both language
      images". There are four language images.
- [ ] `README.md:43`, and `CLAUDE.md:41` and `:53` — the contract is described as
      amended three times, with **ten** conformance cases. Its own header in
      `AlgoJudge-Design/specifications/server-runner/SERVER_RUNNER_API.md`
      records **seven** amendments, and
      `AlgoJudge.Server.Tests/RunnerConformanceTests.cs` holds **thirty** test
      methods.
- [ ] `images/gcc/Dockerfile:38` cites `.claude/rules/runner.md`, and
      `docs/CGROUP_V2.md:8` cites `../../docs/DEVELOPMENT_HOST.md`. Neither is in
      this repository, and the second is in a private one.
- [ ] `.env.example:218` — the `AJ_Pipes__*` block sits *after* the closing *Do
      not set these* section, and its prose has lost words: "Where a judged run
      channels are made", "A judged run output travels", "the job own scratch".
      The key-set check sees none of that.

## After the tag

The five images have to exist before an installation can pull them.
`AlgoJudge-Ops` asks for the moving major `0` and pulls all five by that tag.

`AlgoJudge-External-Runner` can then pin the commit `v0.1.0` names, in place of
the bare revision in its `Cargo.toml`.

The documentation site cuts its `/runner/` snapshot on release day.

## What this file did not check

- **No suite was run against a real daemon here.** What was run on 2026-09-07 is
  `./x test -p aj-runner --lib` — 18 passed, the `.env.example` comparison among
  them — and `./x test -p aj-sandbox --test shim`, 13 passed. The adversarial,
  judging, conformance and end-to-end suites were not, and neither was `./x
  gate` whole.
- **`ghcr.io/algojudge` was not read.** Listing the organisation's packages needs
  a `read:packages` scope this checkout's token does not carry; that the registry
  is empty is somebody else's report.
- **Whether a newer stable Rust, Debian, CPython or PyPy exists** was not
  checked.
