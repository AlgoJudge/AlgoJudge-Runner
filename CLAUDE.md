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
**`Accepted` 2026-08-08; amended 2026-08-09 with §9, unavailability; amended
2026-08-22, §5 and §6; amended 2026-08-24, §3 and §5, Runner tags.** It is the
contract; where this repository and that document disagree, **the document wins
and the difference is reported** rather than worked around. Read its amendment
tables before the body: an amended section still states its pre-amendment form.

> **The list above cited the 2026-08-09 amendment alone until 2026-08-30**, and
> the omission was odd rather than harmless: this file already described the
> other two, under *Where a job's parts come from* and in the `AJ_Runner__Tags`
> bullets, without saying either was an amendment to the contract. Copied from
> the document's own header on 2026-08-30.

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

**`AJ_Runner__Tags` is a seed, not a setting** (2026-08-24). It names the pools
this Runner belongs to, and the Server pairs a Runner with work when the two
lists **share at least one** tag — with an empty list on either side meaning
`default`, so a Runner that names a pool leaves the general queue as well as
joining a reserved one. Two things about it are easy to get wrong here:

- **The Server reads it once, at the first registration.** Every other field
  in `Register` is refreshed when a Runner registers again, which is how a
  restart is reported; this one is not, and the operator owns it in the panel
  from then on. Changing the variable later changes nothing, and that is the
  point: a restart must not move a Runner into an examination's pool.
- **It is skipped when empty**, so a Runner in no pool sends exactly the
  registration it always sent. `tags_are_sent_only_when_there_are_any` is what
  keeps that true.

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

## The problem types

**Two, and the second one is an experiment that stayed.** `standard-io@1` is the
product's principal type; `output-only@1` exists because *adding a problem type
must not require a Server change* was an invariant nothing had ever tested.

The dispatch is a `match` on the problem type in `crates/aj-runner/src/run.rs`,
and the Server's cost is nil: verified 2026-08-30, `output-only` appears nowhere
in `AlgoJudge-Server`'s C# or in `openapi.json`.

**There are two such matches, not one, and this said one until 2026-08-31.**
Judging dispatches on `job.problem_type`; calibration dispatches on
`trial.problem_type` some two hundred lines above it, and the code names it "the
same dispatch as judging, on the same string". The calibration one has a single
arm — `standard-io@1` — so `output-only@1`, the very type this section is about,
falls to its error arm and cannot be trialled. A third type is therefore **two**
arms plus a crate, and a type added by following the old instruction judges but
silently cannot be measured.

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

### Where a job's parts come from (2026-08-22)

A claimed job carries **three opaque documents** and the Server reads none of
them. Which member of each means what is this crate's business:

- **`props`** — what the participant declared. `standard-io@1` reads `language`
  out of it. Absent is an **infrastructure failure**: the job arrived incomplete
  and the submission stays rejudgeable.
- **`problemVersionProps`** — which problem this is. `uva@1` reads its archive
  number out of it; `standard-io@1` needs none. Identity, not settings: it is not
  a layer of the configuration chain.
- **`config`** — the assignment's own, laid over the package's by
  `Config::overlaid`. **One layer**: the Server merges nothing, because
  `ProblemVersion.Config` is gone and there is nothing left to merge.

A language the assignment's `config.languages` excludes is refused **here**, as
the verdict `PolicyViolation` — nothing was offered to a compiler, the code may
be perfect, and what was broken is a rule of the activity. An empty list means
the assignment said nothing, which allows everything this Runner can build.

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

## `output-only@1`

**The participant uploads answers, not a program**, and it was added on
2026-08-09 by `e126fed`, *"Add a second problem type, and leave the Server
untouched"*. `crates/aj-output-only/` is all of it — this file did not name it
until 2026-08-30, despite the crate and the dispatch arm.

It differs from `standard-io@1` on the three axes that could have forced a Server
change, which is what makes it a test of the invariant rather than a variation:
the submission is a file rather than source, the package declares no language and
needs no compiler, and **nothing untrusted is executed**. No build container, no
run container, no policy dictionary — there is no code to read — and nothing that
can escape, because nothing runs.

Four things about it are easy to get wrong:

- **It opens the only untrusted archive in the product.** A package comes from a
  problem author and is semi-trusted; these answers come from a participant.
  `Answers::unpack` goes through `aj_package::extract` under limits of its own,
  tighter than a package's — 2 000 entries, 64 MiB an entry, 256 MiB unpacked, a
  ratio of 200 — so a zip bomb is refused rather than unpacked.
- **A bare file is accepted when the problem has exactly one test**, and refused
  with a sentence naming the test count when it has more.
- **An answer flat in the archive and an answer one directory deep both count.**
  Zipping a selection and zipping a folder are both what people do.
- **A missing answer is a wrong answer, not a failure.** Somebody who answered
  four tests of five is marked on four and told which one is absent; nothing
  crashes and nothing is rejudgeable on that account.

**There is no checker**, deliberately: one would add a sandbox to the one handler
whose point is that it needs none. The checker module is shared with
`standard-io@1` when somebody wants one. `details()` reports compilation as a
warning saying nothing was compiled, rather than `OK`, which would imply
something was.

## Working here

Rust is **not installed on the development host**; `cargo` runs in a container.
Use the wrapper rather than calling `cargo` directly, so everyone builds against
the same pinned toolchain.

Three environment variables are easy to lose an afternoon to. The first two exist
because the Runner starts **sibling** containers; the third does not, and was
introduced under the word "both" until 2026-08-31:

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
