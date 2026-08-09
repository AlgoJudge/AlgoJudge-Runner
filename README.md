# AlgoJudge Runner

Isolated execution and evaluation of submitted solutions for
[AlgoJudge](https://github.com/AlgoJudge).

## Status

**It judges.** This Runner registers, is approved, authenticates, claims jobs,
holds and renews a lease, downloads and verifies packages, unpacks them,
compiles and runs a submission in isolated containers, scores it by groups, and
reports the mark with the compiler's log and a per-test table attached.

`standard-io@1`, in C++ and Python. What is not here yet: the forbidden-word
dictionary, calibration from a model solution, and a second problem type.

## What it is for

The Runner is the component that actually runs untrusted code. It is
deliberately interchangeable: the Server must never depend on any particular
Runner implementation, and several may coexist.

The contract is
`AlgoJudge-Design/specifications/server-runner/SERVER_RUNNER_API.md`, v1.0,
**`Accepted` 2026-08-08**, with nine conformance cases in
`AlgoJudge.Server.Tests/RunnerConformanceTests.cs`.

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

`end_to_end` covers **six outcomes in one run** — accepted, wrong answer, time
limit, compilation error, policy violation, and an infrastructure failure. The
sixth is the one that is not a verdict: a package the Runner cannot open ends
`failed` with **no score at all**, because a zero on a board reads as a wrong
answer about a program that was never run.

## Isolation

**Sibling containers.** The Runner is trusted and holds the container runtime's
socket; the containers that run submissions never do. Each step of the pipeline
— compile, run, check — has its own profile, and every one of them drops all
capabilities, disables the network, mounts a read-only root filesystem, runs as
an unprivileged user, and caps memory, processes, CPU, wall time and output.
One container **per test**, never reused.

`docs/SECURITY.md` is written for the person deploying this: what contains a
submission, **what this arrangement does not buy**, and why the boundary that
holds is the host rather than the container. Two models are rejected there, with
their reasons:
**privileged Docker-in-Docker**, and **passing the socket into the submission
container**. That document also states plainly that mounting a socket read-only
restricts nothing that matters — the boundary is the host, which is why the
evaluation host is treated as compromised by assumption: no secrets,
reproducible, nothing else on it.

`isolate` 2.x is accepted conditionally as the deepest supervisor, after a spike
on cgroup delegation and the capabilities it needs.

**cgroup v2 is required and is checked at start.** The limits are enforced on v1
too — measured, not assumed — but peak memory and CPU time cannot be read
honestly there, and those numbers are shown to a participant beside their
verdict. `AJ_Sandbox__AllowCgroupV1` starts anyway, for a development machine
whose Docker still reports v1, and says so at `ERROR` on every start.

## Security requirements

Every submission is untrusted and assumed hostile: attempts to read system files
and secrets, write outside the working directory, spawn processes, fork-bomb,
exhaust memory or CPU, produce unbounded output, reach the network, survive past
the end of a test, or interfere with another job.

These are held to by an **adversarial suite that runs in CI**, one case per
attack with the outcome it must produce. A suite that is not a gate is not a
gate.

An earlier LXD-based prototype from the engineering thesis is a source of
security **test cases**, not a production specification.

## Related repositories

- [AlgoJudge-Server](https://github.com/AlgoJudge/AlgoJudge-Server) — jobs, packages, results
- [AlgoJudge-Client](https://github.com/AlgoJudge/AlgoJudge-Client) — the web frontend

## License

See [LICENSE](LICENSE). Contributors are listed in [AUTHORS.txt](AUTHORS.txt).
