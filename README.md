# AlgoJudge Runner

AlgoJudge is open-source, self-hosted software for programming contests and
courses, with automatic evaluation of submitted solutions.

This is the component that does the evaluating: isolated execution and marking,
for [AlgoJudge](https://github.com/AlgoJudge).

## What it does

**It judges.** This Runner registers, is approved, authenticates, claims jobs,
holds and renews a lease, downloads and verifies packages, unpacks them,
compiles and runs a submission in isolated containers, scores it by groups, and
reports the mark with the compiler's log and a per-test table attached.

**Two problem types, and eighteen toolchains.** `standard-io@1` in C, C++ and
Python — the table is `crates/aj-standard-io/src/language.rs` — and
`output-only@1`, where the participant uploads answers rather than a program.

**The Server holds nothing about either type.** A problem type costs one `match`
arm in `crates/aj-runner/src/run.rs` and a crate; claiming, leasing, the package
cache, integrity and reporting are shared.

## What it is for

The Runner is the component that actually runs untrusted code. It is
deliberately interchangeable: the Server must never depend on any particular
Runner implementation, and several may coexist.

The Server–Runner contract is at **v1.1**, amended three times. **Ten
conformance cases hold the Server to it**, in
`AlgoJudge.Server.Tests/RunnerConformanceTests.cs`; the protocol is written up
for a reader in [AlgoJudge-Docs](https://github.com/AlgoJudge/AlgoJudge-Docs),
under `/protocol/`.

Three properties of it shape everything here:

1. **The Runner opens an outbound connection.** The Server never calls a Runner,
   which is what lets one sit behind a domestic router with no public address.
2. **There is no socket for a Runner.** The queue is polled, and an empty one
   answers `204` — a normal state, not an error. This is simpler than a socket,
   survives a dropped connection with no reconnection logic, and cannot deliver
   a job twice.
3. **The Runner is stateless apart from a package cache.** One that dies
   mid-evaluation resumes nothing and nobody comes back for that work. The
   Server's **lease** is the whole recovery story: it expires, the job returns
   to the queue, and the Runner that woke up late is refused rather than allowed
   to overwrite whoever holds it now.

## The published images

Pushing a `v*` tag publishes **five images under one version**, to GitHub's
container registry:

```bash
docker pull ghcr.io/algojudge/algojudge-runner:1.2.3
docker pull ghcr.io/algojudge/lang-gcc:1.2.3
docker pull ghcr.io/algojudge/lang-clang:1.2.3
docker pull ghcr.io/algojudge/lang-python:1.2.3
docker pull ghcr.io/algojudge/lang-pypy:1.2.3
```

**All five, because a Runner without the language images judges nothing**, and
because a release is tested as one set: the images and the binary are built,
checked and pushed together, or not at all.

`1.2.3`, `1.2`, `1` and `latest` point at the same image; **a prerelease
(`v1.2.3-rc.1`) publishes only its own tag**, so nothing moving ever points at a
release candidate.

The Runner is `linux/amd64` only. That is not an oversight: cgroup v2 on amd64 is
what the measurement rests on, and a submission's container has to match the
architecture of the host that runs it.

A deployment names the language images explicitly, because the built-in defaults
(`algojudge/lang-gcc:local`) are what the development stack builds locally:

```sh
AJ_Sandbox__Image__Gcc=ghcr.io/algojudge/lang-gcc:1.2.3
AJ_Sandbox__Image__Clang=ghcr.io/algojudge/lang-clang:1.2.3
AJ_Sandbox__Image__Python=ghcr.io/algojudge/lang-python:1.2.3
AJ_Sandbox__Image__Pypy=ghcr.io/algojudge/lang-pypy:1.2.3
```

Each is independent: anything left unset keeps its compiled-in default, so an
operator republishing one image says so in one line.
`AJ_Sandbox__Image__Cpp` is the old name for `…__Gcc` and is still read.

Pin the same version the Runner is, unless there is a reason not to: that pairing
is what the release was tested as.

**`.env.example` lists every variable this Runner reads**, with what each one
defaults to and which of them cannot be turned off by writing `false`. A test
compares it against the source, so a key added to the code and not to that file
reddens the gate.

## How it is built

**Rust**, one static `x86_64-unknown-linux-musl` binary with every backend
compiled in and chosen by configuration, shipped in a minimal image.
`linux/amd64`; **cgroup v2 is required** and is checked at start.

A `.deb` with a systemd unit is supported and **not preferred in production**:
the Runner needs a container runtime anyway, so a package that suggests
otherwise invites installing it where it cannot work.

Rust does not have to be installed to work on this. `cargo` runs in a pinned
container:

```sh
./x build
./x test
./x fmt
./x clippy
```

## What proves it

Nothing here is claimed without a test that runs it.

| Suite | What it proves |
|---|---|
| `cargo test` | the pure parts — the checker contract, comparison, scoring, the archive defences |
| `--test conformance` | the wire protocol, against a real Server, with this Runner as the client |
| `--test adversarial` | the isolation, against real containers, one case per attack |
| `--test judging` | a real submission compiled, run and marked, against the committed package |
| `--test end_to_end` | **the whole product**: a manager publishes, a participant submits, this Runner judges, and what is asserted is what the participant reads |

The last four need a container runtime and are `#[ignore]`d so an ordinary
`./x test` stays fast. All of them run in CI.

## Isolation

**Sibling containers.** The Runner is trusted and holds the container runtime's
socket; the containers that run submissions never do. Each step of the pipeline
— compile, run, check — has its own profile, and every one of them drops all
capabilities, disables the network, runs as an unprivileged user, and caps
memory, processes, CPU, wall time and output. **The read-only root filesystem is
the one that is not universal**: the two build steps ask for a writable root,
because a compiler has to put the program it made somewhere `collect` can read
it back from. They still get no writable *host* path, and the layer dies with
the container. Every step that runs a submission is read-only.
One container **per test**, never reused.

[`docs/SECURITY.md`](docs/SECURITY.md) is written for the person deploying
this: what contains a submission, where the boundary is, and what the evaluation
host is therefore assumed to be — no secrets on it, reproducible, nothing else
running.

**cgroup v2 is required and is checked at start.** The limits are enforced on v1
too, but peak memory and CPU time cannot be read honestly there, and those
numbers are shown to a participant beside their verdict.
`AJ_Sandbox__AllowCgroupV1` starts anyway, for a development machine whose
Docker still reports v1, and says so at `ERROR` on every start.

## Security requirements

Every submission is untrusted and assumed hostile: attempts to read system files
and secrets, write outside the working directory, spawn processes, fork-bomb,
exhaust memory or CPU, produce unbounded output, reach the network, survive past
the end of a test, or interfere with another job.

These are held to by an **adversarial suite that runs in CI**, one case per
attack with the outcome it must produce. A suite that is not a gate is not a
gate.

## Related repositories

- [AlgoJudge-Server](https://github.com/AlgoJudge/AlgoJudge-Server) — jobs, packages, results
- [AlgoJudge-Client](https://github.com/AlgoJudge/AlgoJudge-Client) — the web frontend
- [AlgoJudge-External-Runner](https://github.com/AlgoJudge/AlgoJudge-External-Runner) —
  a second Runner, forwarding submissions to external judging systems. It
  consumes this repository's `aj-protocol` crate over Git, pinned to a revision
  in its `Cargo.toml`, so a breaking change here is a change two repositories
  have to agree on
- [AlgoJudge-Ops](https://github.com/AlgoJudge/AlgoJudge-Ops) — the production
  Compose stack, which is what runs this image at an installation
- [AlgoJudge-Docs](https://github.com/AlgoJudge/AlgoJudge-Docs) — the public
  documentation site, whose `/runner/` and `/protocol/` sections describe this

## Contributing

Open an issue saying what you expected, what happened, and how to reproduce it.
Or open a pull request against `main`: one subject per pull request, with a note
on what changes and why.

By contributing you agree that your work is licensed under the terms below.

## License

This project is licensed under the MIT License.
See [LICENSE](LICENSE).

Authors are listed in [AUTHORS.txt](AUTHORS.txt).
