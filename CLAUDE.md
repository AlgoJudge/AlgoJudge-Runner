# AlgoJudge-Runner

## Scope

A component that executes and evaluates untrusted solutions.
It may be one of several compatible Runner implementations.

## Security rules

- Every submission is potentially malicious.
- Never run a solution without isolation.
- Never run user code as root.
- Do not pass Server secrets into the sandbox.
- Network access is disabled by default.
- Limit CPU, memory, time, processes, files, disk, I/O, and output size.
- Kill the complete process tree.
- Clean temporary resources after success, failure, timeout, and cancellation.
- Results must be safe to submit again.
- Losing the connection must not lose the job. The lease is what makes that
  true, and it is the Server's mechanism — see below.

## Logical contract

A Runner:

1. initiates the connection,
2. authenticates the instance,
3. publishes capabilities,
4. reserves a compatible job,
5. renews the lease,
6. downloads the Runner package,
7. executes the problem-type handler,
8. reports progress,
9. stores the result idempotently,
10. reports infrastructure failures separately from solution verdicts.

## The contract is accepted, and it is not here

`AlgoJudge-Design/specifications/server-runner/SERVER_RUNNER_API.md`, **v1.1**,
**`Accepted` 2026-08-08, amended 2026-08-09** with §9 and unavailability. It is
the contract; where this repository and that document disagree, **the document
wins and the difference is reported** rather than worked around.

Its conformance suite is `AlgoJudge.Server.Tests/RunnerConformanceTests.cs` in
AlgoJudge-Server — **ten cases** a second implementation must also pass. The
tenth arrived on 2026-08-16, when a second implementation found the defect it
was written for: a Runner marking out of one was read as a hundredth.

Three things it **deliberately does not specify**, so nobody adds them back by
accident:

- **There is no WebSocket for a Runner.** The queue is polled and an empty one
  answers **204**. A socket would be an optimisation of *when* a Runner learns
  there is work, never of *how* it takes it.
- **There is no key rotation.** Revocation is permanent; a leaked key means a
  new configuration, a new key and a new registration.
- **A token carries no scope beyond this Runner.**

## Decisions in force (2026-08-06, recorded here 2026-08-08)

- **Rust.** `tokio`, `reqwest`, `serde`, `tracing`, `ed25519-dalek`, `bollard`
  for the container layer. A static musl binary in a `distroless`/`scratch`
  image; a `.deb` with a systemd unit is supported but **not preferred in
  production**. One artefact with every backend compiled in, chosen by
  configuration. `linux/amd64`, **cgroup v2 required**.
- **Three layers.** L1 control: protocol, leasing, cache. L2 backend
  orchestration. L3 the sandbox that runs untrusted code. **We do not write our
  own L3** — execution is delegated to an existing, maintained tool.
- **Our own Docker pipeline is the first backend. Judge0 is out of the MVP.**
  It may return later as an optional backend an operator deploys, reached over
  HTTP. *Supersedes the 2026-08-02 decision that named Judge0 first.*
- **Sibling containers.** The Runner is trusted and may hold the container
  runtime's socket; job containers never do. **Privileged Docker-in-Docker and
  passing the socket into a submission container are rejected** — see
  `docs/SECURITY.md` for why, including the part where mounting a socket
  read-only restricts nothing that matters.
- **`isolate` 2.x is not adopted** (spike run 2026-08-09, `docs/spikes/ISOLATE.md`).
  It works in a non-privileged container, but what it was wanted for — honest
  CPU-time and peak-memory numbers — comes from the sandbox container's own
  cgroup on v2 with nothing granted. **Take the number, not the tool**: read
  `memory.peak` and `cpu.stat`. *Supersedes the 2026-08-06 conditional
  acceptance.*
- **Treat the evaluation host as compromised by assumption**: no secrets,
  reproducible, nothing else on it.
- **`EvaluationJob` is a Server entity.** It exists as a table with a state, a
  lease token, a delivery count and a result. *Supersedes the 2026-08-02
  decision that deferred it onto `Result`.*
- `main` is the integration and default branch.

## `standard-io@1`

`docs/specs/PACKAGE_FORMAT.md` in the workspace, **`Accepted` 2026-08-08**, owns
the package. Two rules are easy to get wrong and matter to a participant:

- **A group's points are awarded only if every test in it passes.** That is what
  makes a group a group rather than a label.
- **The checker's exit code is always 0.** A non-zero code means the *system*
  failed, not that the answer was wrong. Conflating them turns a bug in a
  checker into a rejected submission.

### The language catalogue (2026-08-22)

**Eighteen toolchains in three families**, as a table in
`crates/aj-standard-io/src/language.rs`. Adding a row is a data change; the
build command is a template string handed to the container as opaque argv.

Ids have **two levels** — `cpp17-gcc`, not `cpp17` — because a standard is not a
toolchain and `g++` and `clang++` do not always agree. A header shows the
standard; a submit form offers the toolchain. `cpp` and `python` still resolve,
to `cpp20-gcc` and `python3`, because every package on disk names them.

**Two things are keyed by *family*, not by toolchain, and both fail silently
when that is got wrong**: the forbidden-identifier dictionary and a package's
`overrideLimits`. A lookup that misses returns *no violations* and *no
override* — indistinguishable from a clean submission judged under the package's
own limits. Both now try the id and then the family, and `policy.rs` matches the
family as an enum so a fourth one will not compile until somebody decides what
its rules are.

**Language is required.** It used to default to `cpp`; an absent one is now an
**infrastructure failure**, because it means the job arrived incomplete and the
submission stays rejudgeable. A file whose extension the chosen toolchain does
not accept is the opposite — a **verdict**, `Compilation error`, because the
participant chose both and the compiler would have said so anyway.

Four images carry the eighteen: `images/gcc`, `images/clang`, `images/python`,
`images/pypy`. **Debian, not Alpine** — PyPy has no musl build and Clang plus
static linking is less certain there; trixie rather than bookworm because
`-std=c23` and `-std=c++23` are what the catalogue asks for and GCC 12 spells
them differently. Nothing builds them for you, and every judging case fails
without them.

### What language a message is in (2026-08-09)

**Anything the Runner writes itself is English.** Verdicts, `note`, the
compilation summary, policy rule names, log lines: `Time limit exceeded`,
`Runtime error: segmentation fault (exit code 139)`, `forbidden module os`.

**Anything another system produced travels verbatim**, in whatever language it
arrived in, and is never translated or reworded: the compiler's own output, an
interpreter's traceback, a checker's comment. Those are diagnostics from a tool
that is not us, and rewriting them loses the thing a participant would search
for.

The Runner does not know who is reading, so it does not choose a language for
them — it emits one, consistently, and translation belongs where the reader is.

The forbidden-word dictionary runs **before compilation** and is a **policy
control, not a security control**. It yields `PolicyViolation` with score 0, the
code is never built or run, the participant sees which rule was broken, and the
submission stays rejudgeable.

Do not treat the historical LXD scripts from the engineering thesis as a
production specification. They are a source of security test cases.

## Working here

Rust is **not installed on the development host**; `cargo` runs in a container.
Use the wrapper rather than calling `cargo` directly, so everyone builds against
the same pinned toolchain.

Two environment variables exist because the Runner starts **sibling**
containers, and both are easy to lose an afternoon to:

- `AJ_DOCKER_SOCKET=1` lets `./x` hand the build container the runtime socket,
  which the isolation and judging suites need. Off by default, because anything
  that can reach that socket is root on the host.
- `AJ_Work__HostPath` is the job scratch directory **as the daemon sees it**. A
  bind mount is resolved by the daemon, so a path that is real to a
  containerised Runner and meaningless to the daemon produces an **empty
  directory** rather than an error — and tests then run against nothing.
- `AJ_Sandbox__AllowCgroupV1` starts on a host the Runner would otherwise
  refuse. Development only, and it says so at `ERROR` on every start; a quiet
  override is a production setting waiting to happen.

Two things about the socket, both learned the hard way. Docker Desktop's socket
is `root:root` mode **755** — writable only by root — so a non-root container
cannot use it and there is no group to join; on Linux it is `root:docker` mode
660 and `group_add` is the answer, which keeps the process unprivileged. And a
round is opened by the scheduler's scan rather than by the request that created
it, so a submission sent in that gap is a genuine 404 and a test waits rather
than retrying.

When this repository is checked out inside the AlgoJudge workspace,
`../PROJECT_CONTEXT.md` is the primary architecture context and takes precedence
over this file — **except where it still names Judge0 as the first backend**,
which the 2026-08-06 decisions above supersede.
