# Releasing the Runner

For whoever cuts the release. Somebody installing the product wants
[AlgoJudge-Ops](https://github.com/AlgoJudge/AlgoJudge-Ops) instead.

Every dated claim below was checked on **2026-09-20** against `release/0.2.0` at
`fee2cde`. Where something could not be checked, this file says so rather than
leaving it to be read as verified.

## There is a released version, and `0` moves onto this one

`v0.1.0` is tagged and `ghcr.io/algojudge` holds all five images under `0.1.0`,
`0.1`, `0` and `latest`. Three things follow that did not apply to a first
release:

- **`0` is what `AlgoJudge-Ops` asks for**, for the Runner and its four language
  images together. An installation following the moving major crosses onto this
  version on its next `update.sh`, without being asked. The release note is the
  only warning its operator gets.
- **A tag re-points four names.** `0.2.0` is new; `0.2`, `0` and `latest` are
  moved off the previous release's bytes. The version check before the push is
  the only thing standing between a typo and a moving tag pointing backwards.
- **An upgrade path exists and is reasoned about.** What a running installation
  carries in its `.env` and its volumes has to keep working, or the note has to
  say it does not.

`v0.0.1-rc.1` was deleted from this remote in August 2026 and
`ghcr.io/algojudge/algojudge-runner:0.0.1-rc.1` and `lang-python:0.0.1-rc.1` are
still served. **Deleting a tag unpublishes nothing.**

## Where the version lives

**`Cargo.toml`, `[workspace.package]`, one line.** All six crates inherit it
with `version.workspace = true` — checked in all six manifests — so a release
changes that line and `Cargo.lock` follows. Check the lock in, do not hand-edit
it. Both say `0.2.0`.

`README.md` names the version in **ten** lines: five `docker pull`, four
`AJ_Sandbox__Image__*`, and the sentence listing the four moving tags. `git grep
-n` the previous version rather than trusting a line number.

Two places name a version and are **not** swept:

- `crates/aj-runner/src/images.rs` says the bug it closes shipped in `0.1.0`.
  That is a statement about a released version and stays true.
- The prerelease illustration in `README.md` names the version being released,
  because a prerelease of some older line is not what a reader is about to cut.

## The tag does not go on a release branch

**CI runs on `main` and on pull requests into it.** `.github/workflows/ci.yml`
triggers nowhere else, so no commit on a `release/*` branch has a run of its own
and `gh run list --branch release/<version>` is empty by construction.

**`release.yml` refuses a tag that is not an ancestor of `main`.** The branch is
merged by pull request first, and the tag goes on the `main` commit that
results, once *that* commit's own CI run is green.

## This Runner is released before the external Runner

`AlgoJudge-External-Runner` takes the `aj-protocol` crate from here, pinned in
its `Cargo.toml` and `Cargo.lock` to a **revision** — a commit, not a branch and
not a moving tag. Its manifest gives the reason: *AlgoJudge-Runner publishes no
tags, so a revision is the only stable name.*

That reason has expired, and the pin has not caught up. On 2026-09-20 it names
`9f25532c`, a merge on this repository's `main`, and `crates/aj-protocol` has
changed three times since it. **What this release owes that repository is the
commit its tag points at**, and the Rust base digest below, which its own
checklist requires to equal this one's.

Moving the pin is that repository's release step, not this one's:

```sh
git -C ../AlgoJudge-Runner rev-parse vX.Y.Z^{commit}
```

then `./x build` there to rewrite its lock. It is done when the forty characters
in its `Cargo.toml` are what that printed and the same forty appear twice in its
lock.

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

Each gets `<version>`, `<major>.<minor>`, `<major>` and `latest`. **A prerelease
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
  Verified 2026-09-20: five files across the two repositories, one digest.
- **Stable Rust is ahead of the pin.** 1.98.1 was released 2026-09-03; the pin
  holds 1.97.1. A digest says what it is, never what it is behind, so this is a
  question for the release channel and it is asked at every release.
- **The four language images pin nothing.** Their bases are `debian:trixie-slim`
  — gcc, clang, and the shim stage in all four — `python:3-slim` and
  `pypy:3-slim`, which are tags, so the same commit can build different
  compilers on different days while a problem author sets limits against one of
  them. **What a release shipped is read off the images it built**, never copied
  from here: `docker run --rm <image> cat /etc/algojudge-toolchain` prints it.

### Every image this repository pins, and how each one moves

Eleven `FROM` and `image:` lines, five distinct images, in seven files. The
checklist below asks whether a newer one exists; this table says where to look
and what the answer is worth.

| | where | how it moves |
|---|---|---|
| `rust@sha256:3b2879…` | `Dockerfile`, `Dockerfile.toolchain`, `ci.yml` | **digest, three places** — and `AlgoJudge-External-Runner` pins the same one in two more. Five copies of one decision |
| `gcr.io/distroless/static-debian13:nonroot` | `Dockerfile` | a tag, and **the base of the image this repository publishes**. Debian 13, as the language images are |
| `debian:trixie-slim` | four language `Dockerfile`s, six lines | a tag. The shim stage of all four, and the runtime of gcc and clang |
| `python:3-slim`, `pypy:3-slim` | one each | tags, and **which interpreter a submission meets** |
| `postgres:18` | `example-runner-development-docker-compose.yaml` | a tag, major pinned on purpose — development only, nothing published depends on it |

**A newer base is not automatically a better one here.** The four language
images decide what a submission is compiled and run by, and a problem author's
limits were measured against one compiler. Raising them is a decision with a
date, made between releases rather than inside one.

## The dependencies

**`cargo audit` is the only thing that looks for advisories, and nothing in CI
runs it.** So the last time somebody typed it is the last time this was asked.
It is not in the toolchain image either: install it into the `./x` cargo volume
first.

```sh
./x install cargo-audit --locked && ./x audit
```

It **exits non-zero on a finding**, so it belongs in a chain rather than in a
glance at the output.

**Run it before the version bump, not after.** At 0.2.0 it found
`RUSTSEC-2026-0285` — a TLS 1.3 handshake flaw in `rustls` 0.23.44, medium,
published 2026-09-14, six days before the release and a week after the previous
run. `rustls` is transitive, through `reqwest` and `hyper-rustls`, and it is the
TLS the Runner speaks to the Server. The fix was `0.23.45`: a patch **inside the
range already in `Cargo.toml`**, so `./x update -p <crate>` took it and moved
nothing else.

That is the shape worth expecting — an advisory against a transitive crate,
closed by a patch the existing range already allows. Check it with
`./x update --dry-run -p <crate>` before applying, so that *one package* is a
reading rather than a hope.

**`Cargo.lock` is behind inside its own ranges**, and further behind across
majors, which would need a range changed in `Cargo.toml`. `./x update --dry-run`
says by how much on the day. Being behind is not by itself a reason to move.

**A Dependabot pull request that is red is not deferred work.** Two majors of
load-bearing crates — the container client and the signature crate — were open
and failing `build` and every container job at 0.2.0. A major upgrade is not
taken under release pressure; it is its own piece of work, and the release goes
out without it. That is a different judgment from the paragraph above: a patch
that closes an advisory inside its range is taken, and a major that breaks the
build is not.

## How far `.env.example` is checked

`crates/aj-runner/src/config.rs`,
`every_variable_the_config_reads_is_in_the_example_and_no_others`, runs in
`cargo test --workspace`. **It compares the key set in both directions and
nothing else**, and four limits are worth knowing before a green run is trusted:

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

**Read the prose too**, and read where each block sits: a section that drifts
below the file's closing *Do not set these* is invisible to a check that
compares key sets, and so is a sentence that has lost a word.

### What a release owes an existing `.env`

A key set that grows is an upgrade an operator does nothing for. A key that is
**renamed or removed** breaks a file already on a disk, and nothing in this
repository reads that operator's file to tell them. So diff the key set against
the previous tag and put the answer in the release note:

```sh
git show v<previous>:.env.example | grep -oE '^#?\s*AJ_[A-Za-z_0-9]+=' | tr -d '# =' | sort -u
```

against the same over the working tree. At 0.2.0 that was five added, none
removed, none renamed, and every new key defaulting to the previous behavior.

## Before the tag

### The version

- [ ] `Cargo.toml` says the version being released, and `Cargo.lock` agrees —
      six `aj-*` entries, rewritten by `./x build` and never by hand.
- [ ] `README.md` names that version in the ten lines above.
- [ ] `docs/RELEASE.md` — this file — states the readings of the day it is
      being cut, not of the last release.
- [ ] `release/<version>` is merged into `main` by pull request, and the tag
      names a commit on `main` whose **own** CI run is green.

### The gate

`./x gate` is the `build` job of `ci.yml`, in the same order, against the same
pinned toolchain. Rust is not a prerequisite; `./x` runs cargo in a container.
**`./x` is a POSIX `sh` script**: Git Bash or WSL, not PowerShell.

- [ ] `./x gate` — `cargo fmt --all --check`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo build --workspace --release`,
      `cargo test --workspace`.
- [ ] That suite carries the `.env.example` comparison above. A green run is
      parity of the **key set**; read the comments and the sample values too.
- [ ] `./x install cargo-audit --locked && ./x audit`, and record the date.
      Nothing in CI runs it, so the last run is the only one there is.
- [ ] The Rust base digest in `Dockerfile` and `Dockerfile.toolchain` is the one
      intended, and **the same digest the external Runner pins**. Two Rust images
      a month apart is two compilers nobody chose.
- [ ] **Every image in the table above has been looked at**, and what is behind
      is behind for a reason somebody wrote down. `grep -rn '^FROM ' Dockerfile*
      images/*/Dockerfile` and `grep -rn 'image:' example-*.yaml
      .github/workflows/*.yml` list them; a digest says what it is and never
      what it is behind, so this is a question to ask the registry rather than
      the file. Record the answer and the date, whether or not anything moves.
- [ ] `.env.example` is the only `.env*` in the repository. `.gitignore` ignores
      `/.env` and `/.env.*` and re-admits the example alone, a pattern that has
      swallowed it before.

### The containers, against a real daemon

Everything here needs a container runtime, is `#[ignore]`d so that an ordinary
`./x test` stays fast, and needs `--test-threads=1` because these fight over the
daemon when run in parallel.

**The adversarial suite is what stands between a release and a sandbox that does
not isolate** — one case per thing untrusted code will try. The conformance
suite is the other half: the wire protocol, against a real Server.

**CI runs every one of these, on both cgroup drivers, on the commit itself.**
`integration` and `adversarial` are each a two-leg matrix, and each leg asserts
the driver it was given rather than trusting the daemon to have taken it. So the
question before a tag is not whether to run them again — it is whether **that
commit's own run** is green on all six jobs.

Run them here when a local change is being judged, or when CI's answer is in
doubt. On a host whose daemon is not Linux with cgroup v2 they are **weaker**
evidence than the run already on the commit: one driver instead of two, and a
virtual machine instead of the host the measurement rests on.

- [ ] Build the four language images first. Nothing builds them for you, and
      every judging case fails at its first line without them — as does the
      development stack, because the Runner proves it can judge before it
      registers and this compose file names none of the images:

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
- [ ] **The stack builds the Server from `../AlgoJudge-Server`**, and CI checks
      that repository out at its default branch. So a conformance run proves this
      Runner against whatever the Server's `main` is that day — which on a
      release day is the Server's own released commit, and is the pairing worth
      having. Say which Server commit answered.
- [ ] The image builds and the toolchains answer. `ci.yml`'s `docker` job builds
      the Runner image and nothing else; the four language images are built in
      its `integration` and `adversarial` jobs, and the step that runs each
      toolchain — `g++`, `gcc`, `clang`, `python3`, `pypy3` — is in
      `release.yml`, where it gates the push on the tag and nothing earlier.

## Cutting the tag

The tag is what publishes; nothing that lands on `main` reaches the registry on
its own. Three things are true before it is cut, and each is read rather than
assumed:

```sh
git merge-base --is-ancestor <sha> origin/main              # it is on main
gh run list -R AlgoJudge/AlgoJudge-Runner --commit <sha>    # its own run, green
git tag --list                                              # the name is free
```

**Its own run.** A later green run on `main` is evidence about a later commit,
and a release branch has no run at all. That middle command has also returned
**nothing at all** for commits whose runs existed and were green; when it does,
read the run by id rather than concluding there was none.

**Six jobs, not one.** `build`, `docker`, `integration` on both cgroup drivers
and `adversarial` on both. A green `build` alone is the compiler's opinion and
says nothing about whether the sandbox still isolates.

The tag is annotated, and the message names the product:

```sh
git tag -a v<version> -m "AlgoJudge Runner <version>" <sha>
git push origin v<version>
```

That push starts `.github/workflows/release.yml`, which takes about **four and a
half minutes** — the longest release run in the product, because it builds and
pushes five images where the others push one. It starts within about twenty
seconds of the tag landing. Watch it — `gh run watch <id>` — rather than
assuming it.

**Two things have no undo.** The run is never canceled: `cancel-in-progress` is
`false` here because a run interrupted between two `docker push` calls leaves a
version half in the registry, and with five images there are far more places to
be interrupted. And **deleting a tag unpublishes nothing**. The name is checked
before the push or not at all.

Then the GitHub Release, which no workflow creates — `release.yml` holds
`contents: read`:

```sh
gh release create v<version> -R AlgoJudge/AlgoJudge-Runner --title "<version>" --notes-file <file>
```

The title is the bare version, no `v`. `--prerelease` when the version carries
one; a prerelease publishes its own tag alone and moves the major, the minor and
`latest` onto nothing.

**A release body is not a file in this repository.** GitHub renders a single
newline as a line break, so each paragraph is written as one long line.

## After the tag

The five images have to exist before an installation can pull them. Read all
five back, and read them **anonymously** — listing the organization's packages
needs a `read:packages` scope a checkout's token does not carry:

```sh
token=$(curl -s "https://ghcr.io/token?scope=repository:algojudge/<image>:pull&service=ghcr.io" \
  | python -c "import json,sys; print(json.load(sys.stdin)['token'])")
curl -s -H "Authorization: Bearer $token" "https://ghcr.io/v2/algojudge/<image>/tags/list"
```

A package created by its first push is **private**, and no workflow and no token
here can change that. All five were public at 0.2.0, so only a newly named image
would need somebody with access to the organization's packages to act.

`AlgoJudge-Ops` asks for the moving major `0` and pulls all five by that tag.

`AlgoJudge-External-Runner` moves its `aj-protocol` pin onto the commit the new
tag names.

The documentation site cuts its `/runner/` snapshot **on release day** —
`npm run snapshot -- v<major>.<minor> runner` in `AlgoJudge-Docs`. It takes a
minor, refuses a patch version as usage, and **there is no backfill**. A patch
release must not invoke it.

### The public website states this component's version

`algojudge.pl` prints **`Runner v<version>`** in four places — a card badge and a
roadmap item, in each of `src/content/pl.json` and `src/content/en.json` of
`AlgoJudge-Website`. A release makes all four wrong.

**`AlgoJudge-Website` has no CI.** Its tests run only when somebody types `npm
test`, so nothing reports the mismatch.

`AlgoJudge-Website/tests/content.test.mjs` pins the version literal by regex and
hard-codes the five repository keys. Correcting the content turns that suite red:
the test asserts the literal and needs the same edit. Change content and test in
one commit.

The correction is `/website-sync` in the workspace. This runbook's step is to
record that it is owed.

## What this file did not check

- **No suite was run against a real daemon for this file's figures.** The
  container suites' evidence at 0.2.0 is the CI run on the tagged commit, on both
  cgroup drivers — cited as that, not as a local run.
- **Whether a newer Debian, CPython or PyPy exists** was not checked. That the
  three tags still resolve was.
- **What the four language images shipped** is not recorded here on purpose. It
  is read off the images the release built.
