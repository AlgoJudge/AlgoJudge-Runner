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

`AlgoJudge-Design/specifications/server-runner/SERVER_RUNNER_API.md`, v1.0,
**`Accepted` 2026-08-08**. It is the contract; where this repository and that
document disagree, **the document wins and the difference is reported** rather
than worked around.

Its conformance suite is `AlgoJudge.Server.Tests/RunnerConformanceTests.cs` in
AlgoJudge-Server — nine cases a second implementation must also pass.

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
- **`isolate` 2.x is accepted conditionally**, after a spike on cgroup
  delegation and the capabilities it requires.
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

When this repository is checked out inside the AlgoJudge workspace,
`../PROJECT_CONTEXT.md` is the primary architecture context and takes precedence
over this file — **except where it still names Judge0 as the first backend**,
which the 2026-08-06 decisions above supersede.
