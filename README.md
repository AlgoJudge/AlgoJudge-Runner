# AlgoJudge Runner

Isolated execution and evaluation of submitted solutions for
[AlgoJudge](https://github.com/AlgoJudge).

## Status

**Not implemented.** This repository currently holds a licence, a contributor
list and its development instructions — no code, no build, no tests.

It is published so the intended architecture is visible while the component is
designed. Nothing here is usable yet.

## What it is for

The Runner is the component that actually runs untrusted code. It is deliberately
interchangeable: the Server must never depend on any particular Runner
implementation, and several may coexist — a native one, a Judge0 adapter, one
per specialised task type.

The intended contract:

1. the Runner opens an **outbound** connection to the Server, so it can live
   behind NAT and needs no public address
2. it authenticates as a machine client and publishes its capabilities —
   supported task types, languages, tags
3. it claims a compatible job, downloads the runner package and verifies it
4. it prepares an isolated environment, compiles, runs the tests, applies limits
5. it reports progress, then submits the result **idempotently**
6. infrastructure failures are reported separately from solution verdicts

## Decisions taken

- **The first version uses Judge0 as the sandbox.** The coupling to Judge0
  belongs here, never in the Server.
- `EvaluationJob` is deferred as a Server entity. Evaluation is tracked on
  `Result`, which names the Runner that is evaluating or has evaluated a
  submission.
- The Server–Runner protocol is a **proposal**, not an accepted specification.
  It lives in `AlgoJudge-Design` as `proposals/Server-Runner-api.md`, status
  `Proposed`. It does not yet cover atomic job reservation, leases, heartbeat,
  cancellation, versioning or key rotation — all of which have to be settled
  before this is built.

## Security requirements

These are requirements, not descriptions of existing behaviour. Every submission
is untrusted and must be assumed hostile: attempts to read system files and
secrets, write outside the working directory, spawn processes, fork-bomb,
exhaust memory or CPU, produce unbounded output, reach the network, survive past
the end of a test, or interfere with another job.

The minimum bar: an isolated environment, no network by default, restricted
privileges, a separate working directory, limits on CPU, wall time, memory,
processes, disk and output, killing the whole process tree, no secrets inside
the sandbox, cleanup after every outcome including timeout and cancellation, and
audit logging.

An earlier LXD-based prototype from the engineering thesis is a source of
security **test cases**, not a production specification.

## Related repositories

- [AlgoJudge-Server](https://github.com/AlgoJudge/AlgoJudge-Server) — jobs, packages, results
- [AlgoJudge-Client](https://github.com/AlgoJudge/AlgoJudge-Client) — the web frontend

## License

See [LICENSE](LICENSE). Contributors are listed in [AUTHORS.txt](AUTHORS.txt).
