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
- Losing WebSocket must not lose the job.

## Logical contract

A Runner:

1. initiates the connection,
2. authenticates the instance,
3. publishes capabilities,
4. reserves a compatible job,
5. renews the lease,
6. downloads the Runner package,
7. executes the task-type handler,
8. reports progress,
9. stores the result idempotently,
10. reports infrastructure failures separately from solution verdicts.

## Decisions in force (2026-08-02)

- **The repository is empty.** It holds a LICENSE, an `AUTHORS.txt` and this
  file. There is no implementation, no build and no tests yet.
- **The first version uses Judge0 as the native sandbox.** The Server must still
  not depend on Judge0; that coupling belongs here.
- **`EvaluationJob` is deferred as an entity.** The Server tracks evaluation on
  `Result`, which names the Runner that is evaluating or has evaluated a
  submission. Atomic reservation, lease deadlines and idempotent result
  submission still have to be solved — the deferred name does not defer them.
- The Server–Runner contract is a **proposal**, not an accepted specification:
  `proposals/Server-Runner-api.md` in AlgoJudge-Design, status `Proposed`. It
  does not yet cover reservation, leasing, heartbeat, cancellation, versioning
  or key rotation.
- `main` is the integration and default branch.

Do not treat the historical LXD scripts from the engineering thesis as a
production specification. They are a source of security test cases.

## Working here

When this repository is checked out inside the AlgoJudge workspace,
`../PROJECT_CONTEXT.md` is the primary architecture context and takes precedence
over this file.
