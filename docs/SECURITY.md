# Security

> What this Runner does to contain untrusted code, what it deliberately does
> not do, and where the boundary that actually holds is. Written for the person
> who has to deploy it.

The one sentence to take away: **the boundary is the host, not the container.**
Everything below narrows what a submission can reach; none of it makes the
evaluation host safe to share with anything else.

---

## 1. Treat the evaluation host as compromised

Not as a slogan — as a deployment instruction.

- **No secrets on it.** No Server credentials, no registry tokens, no `.env`, no
  keys for anything but the Runner's own identity.
- **Nothing else on it.** Not the Server, not the database, not a reverse proxy
  for something you care about.
- **Reproducible.** You should be able to destroy and rebuild it without
  thinking, because that is the response to any suspicion.
- **No lateral reach.** It should not be able to open a connection to anything
  in your network that it does not need. It initiates one outbound connection,
  to the Server, and nothing else.

The reason is §3.

## 2. What contains a submission

Each step of the pipeline runs in its own container, started by the Runner
through the container runtime's API. **One container per test, never reused** —
state carried between tests is a thing untrusted code tries, and a fresh
container is the only answer that does not depend on cleanup having been written
correctly.

| | Applied to every step that runs a submission |
|---|---|
| `--network=none` | no route anywhere |
| `--cap-drop=ALL` | no capabilities |
| `--security-opt=no-new-privileges` | and none may be gained |
| read-only root filesystem | writes go nowhere it covers — see `/dev/shm` below |
| `--user 65534:65534` | never root, even inside |
| `--memory` **with `--memory-swap` equal** | without the second the limit means nothing: the process swaps instead of being killed. **Two sources say a kill happened** and either is enough — see below |
| `--pids-limit` | a fork bomb hits a wall |
| `--cpus` | one processor's worth per second. The threading hole is closed by the accounting as well — `cpu.stat` sums the subtree, so threads spend the budget faster rather than escaping it |
| `--cpuset-cpus`, **only where the Runner was given a set** | an operator's division of the host, carried to the job containers, which inherit no affinity of their own. Given the whole machine the Runner pins nothing: several Runners choosing processors with nothing coordinating them is worse than letting the host place the work |
| wall clock = **three times the limit plus one second without progress** | not a limit anybody is judged against: a time limit is processor time, so this reaps what is *not* spending any — one stuck in an uninterruptible syscall, or one that waits rather than computes. The deadline stops counting whenever the processor time grows, so a program descheduled on a busy host is never reaped for it |
| processor time past **twice the limit** | there is no reason to keep waiting; the verdict is still decided afterwards on the measurement |
| an output cap enforced **while it runs** | read afterwards, a flooding program fills the host's disk first |

**No step that runs a submission is given a tmpfs.** The two profiles that ask
for one are the submission build and the checker build, and both ask for a
writable root on the next line — so the two properties are mutually exclusive
across the pipeline.
A running submission's only writable path is `/dev/shm`, which the contract below
describes as a surface nobody declared and rule 3's test names explicitly.

**A memory kill is told from two places.** The
container runtime reports `OOMKilled` on the container, and the kernel counts in
the run's own cgroup — `memory.events`, read beside `memory.peak` and
`cpu.stat`. Either source is enough, and neither is checked against the other.

**The kernel's half is two fields and needs both**, because each alone says the
wrong thing — the definitions are the kernel's own:

| Field | *"…"* | Alone it would |
|---|---|---|
| `oom_kill` | *the number of processes belonging to this cgroup killed by **any kind of OOM killer*** | blame a submission for a **host** that ran out of memory |
| `oom` | *the number of time the cgroup's memory usage **was reached the limit** and allocation was about to fail* | report a limit reached but survived — measured on one slice after a term of judging, `oom 845` against `oom_kill 843` |

Together they are a program killed for exceeding the limit it was given.
`memory.events` and not `memory.events.local`, because the container runs in a
*child* of the cgroup this reads and only the first is hierarchical.

**Two sources because the runtime's flag is not reliable on its own.** It has
been observed reporting `OOMKilled` false for a container that exited **137
after 117 ms** on a systemd-driver host. That is not a missing number but a
**wrong verdict** — a memory limit told to a participant as a runtime error —
and `Stopped::Memory` is the only route to that verdict, so nothing else would
catch it. It does not recur in 25 attempts, so it is a rare race in the runtime
rather than a broken flag; a second opinion is the cheap answer to a rare one.

Every row has a test in `crates/aj-sandbox/tests/adversarial.rs`, and each
asserts two things: the program was stopped correctly, **and the host is
unchanged afterwards**. A sandbox that contains a program by leaking a process
has not contained it.

**One of those four taught something that outlives it.** The obvious capability
test — assert the effective set is empty — is no test at all here. Measured with
`cap_drop` deleted and nothing else changed: `CapInh`, `CapPrm`, `CapEff` and
`CapAmb` stay all zeros, because the kernel clears them on the drop to uid 65534.
Only `CapBnd` moves, to `00000000a80425fb`. The bounding set is the assertion
that gates, and it is also the one that matters: it is the ceiling on what may
ever be *regained*.

Two more, from the pipeline rather than the sandbox:

- **The build gets no writable host path.** It writes to its own container layer
  and the artefact is read back through the runtime API. The alternative was
  opening a directory to every user on the host.
- **A package-supplied checker is sandboxed too.** It comes from a problem
  author rather than from the platform, and it runs with its own limits and no
  network, never in the Runner's process.

### What one test is, stated as a contract (2026-08-09)

Four rules. They are cheap to hold and expensive to notice the loss of, so each
has a test rather than a paragraph.

1. **One test, one container, never reused.** A fresh container is the only
   answer that does not depend on cleanup having been written correctly.
2. **The program is given its own test's input and nothing else.** One file:
   `<name>.in`, mounted read-only, and the program is started as
   `exec … < /in/<name>.in`. Mounting the whole `tests/` directory would put
   `<name>.out` — the answer — inside the submission's own container. See
   `pipeline.rs::input_mount`.
3. **Nothing a program writes reaches the next test.** Asserted in
   `adversarial.rs::nothing_survives_from_one_run_to_the_next`, for the scratch
   tmpfs **and** for `/dev/shm`.
4. **The checker is contained on the same terms.** Same `Sandbox::run`, so the
   same table above applies to it, with its own limits. A checker stopped by a
   limit is reported as a **broken checker**, never as a wrong answer.

Two things that follow, and are easy to get wrong in the opposite direction:

- **`/dev/shm` is writable and the profile does not ask for it.** The runtime
  mounts a 64 MiB tmpfs there in every container and a read-only root filesystem
  does not cover it. It breaks none of the four — it is new with the container,
  and tmpfs pages are charged to the memory limit, so a program spending it is
  spending its own budget — but it is a surface nobody declared, which is why
  rule 3's test names it explicitly.
- **The input is a mounted file, not a pipe.** A pipe would be marginally
  stricter and is deliberately not used: it is **not seekable**, so a solution
  that reads its input twice would work on the author's machine and fail here.
  The access surface is already one test either way, so the stricter option buys
  nothing and costs a participant a verdict they cannot explain.

## 3. What this does **not** buy — read this part

**The Runner holds the container runtime's socket, and anything that can reach
that socket is root on the host.** It can start a privileged container that
mounts `/`. That is not a flaw in the arrangement; it is the arrangement.

Three consequences, stated so nobody has to discover them:

- **Mounting the socket read-only restricts nothing that matters.** The flag
  applies to the socket *file*, not to the API spoken over it.
- **A bug in the Runner is a host compromise**, not a container compromise. The
  Runner is trusted code; the containers it starts are where the untrusted code
  goes, and they never get the socket.
- **This is reducible, not safe.** The path runs through an API that can be
  proxied, scoped, moved to rootless Podman, or pushed onto a separate machine
  reachable over mTLS. Each of those narrows it. None removes §1.

### Two arrangements that are rejected

**Privileged Docker-in-Docker.** The Runner would run `--privileged` and start a
nested daemon. The privilege the *infrastructure* needs then becomes the
privilege available to anyone who escapes the inner sandbox — which is exactly
how the Judge0 CVE chain turned an arbitrary-file-write bug into host
compromise. `isolate` was not the flaw; running it inside a privileged container
is what made the flaw fatal.

**Passing the socket into the submission container.** This hands untrusted code
the host directly. It is not a weaker version of the sibling model; it is a
different mistake, and the rule is one line for code review: *the socket goes to
the Runner and to nothing the Runner starts.*

### The cost of the sibling model, which has to be designed for

Job containers are **siblings**, so they outlive the process that made them. The
Runner labels every container it starts and **sweeps orphans at startup**;
without that, a crash-loop fills the evaluation host with dead sandboxes until
it runs out of disk.

## 4. The forbidden-identifier dictionary is not a security control

It is a **policy** control, and the difference is not pedantry.

It exists to catch a violation of the activity's rules **early**, so a
participant learns they broke a rule instead of finding out from a results
table. It runs before the build, gives `PolicyViolation` with score 0, names the
rule, and leaves the submission rejudgeable.

**Every rule in it is expected to be bypassable**: token pasting, macro
indirection, runtime name construction with `dlsym`, inline assembly, raw
syscalls, encoding tricks. This is the project's own conclusion from its
engineering thesis, and it is the consensus elsewhere — none of `isolate`,
`nsjail` or `sio2jail` implements source filtering at all.

A submission that gets past it is still contained by §2. A document that
suggests otherwise is teaching a false sense of safety.

## 5. cgroup v2 is required, and why

Checked at start; the Runner refuses without it. **`docs/CGROUP_V2.md` states the
minimum a host has to satisfy** and how to check it; this section is the reason.

**The limits are enforced on v1** — measured, not assumed: a container over its
memory limit is OOM-killed there and `OOMKilled` is reported. What v1 cannot do
is **measure**: `memory.peak` and `cpu.stat` are v2 interfaces, and `isolate`
2.x dropped v1 outright.

**That is a refusal to judge, not a missing number.** A time limit is decided on
processor time read from `cpu.stat`, so the refusal covers three conditions:
cgroup v1, a cgroup driver this Runner knows neither of, and a cgroup tree it
cannot use.

**Measuring at all needs the Runner's own container to run as root under
`cgroupfs`, and only the peak-memory number needs it under `systemd`** — the
tree's directories are root's, and `cgroupfs` is the backend that creates one.
Running as root is a decision rather than a slip: the Runner holds the container runtime's socket, which is root-equivalent on the
host by §1 of this document, so the uid inside its container was never the
boundary. Nothing it *starts* gains anything by it — a job container still runs
as `65534:65534` with every capability dropped.

`AJ_Sandbox__AllowUnmeasured` starts anyway — **and only starts**. Such a Runner
registers and answers the protocol and then fails every job it claims with an
infrastructure error, which is what a conformance suite needs and all it needs.
It is off by default and shouts at `ERROR` on every start, because a quiet
override is a production setting waiting to happen. `AJ_Sandbox__AllowCgroupV1`
is the old name and is still honoured.

## 6. What is not here yet

Stated so that absence is not read as a decision:

- **A custom seccomp profile.** Docker's builtin profile applies and does real
  work — measured 2026-08-09: it refuses `keyctl`, `add_key`, `bpf`,
  `perf_event_open`, `mount`, `setns`, `io_uring_setup`, `open_by_handle_at` and
  **`unshare(CLONE_NEWUSER)`**, which without the profile *succeeds*.

  **Check that it is on.** `docker info` must report
  `name=seccomp,profile=builtin`, and a process in the sandbox must show
  `Seccomp: 2` in `/proc/self/status`. Docker Desktop 24.0.7 on this workstation
  reported **`profile=unconfined`** — no syscall filtering at all, while this
  document claimed otherwise. Upgrading to 29.x turned it on. A profile nobody
  verified is a profile nobody has.

  **What a custom profile would add, with one concrete reason.** `isolate`'s own
  filter is allow-by-default with four rules, and Docker already covers three:
  `keyctl`, `AF_VSOCK` and `io_uring_setup`. The fourth it does not cover is
  **file locks** — `flock` and `fcntl` with `F_SETLK`/`F_OFD_SETLK`/`F_SETLEASE`.
  Locks are shared across mount-namespace boundaries on a shared inode, and that
  is **demonstrated here, not theoretical**: two containers with the same volume
  mounted read-only see each other's locks, one holding a shared lock and the
  other refused an exclusive one.

  **Corrected 2026-08-09: our sandboxes do not share an inode, so this does not
  reach us.** An earlier version of this section said two submissions to the same
  problem share the mounted tests, because the package is cached per problem
  version. What is cached is the **archive**; the extraction target is
  `Scratch::new(…, job_id)`, a directory per job, so two concurrently judged
  submissions unpack their own copies and lock nothing in common. The channel is
  real where a file *is* shared — which is why it is written down — and the
  architecture that would expose it is one where the unpacked package is mounted
  from the cache. It is not.

  So the lock rule has **no concrete driver here today**, and a custom profile
  goes back to being defence in depth without a named threat. What would bring it
  back is a change nobody would think of as a security decision: mounting the
  unpacked package from the cache instead of unpacking per job, to save the copy.
  That would be a sensible-looking performance change and would open a
  contestant-to-contestant channel.

  **Decided 2026-08-09: the property is pinned instead of the syscall.**
  `run.rs::two_jobs_mount_nothing_in_common` asserts that every path a sandbox
  mounts comes from the job's own scratch, and it was checked by breaking the
  invariant on purpose and watching it fail.

  Denying `flock` would close one road and leave POSIX record locks, `F_NOTIFY`
  and anything else two processes can do to one inode. What protects us is that
  there is no shared inode to hold, so that is what is guarded.

  The other half of the trade is what a custom profile costs. Docker's
  `--security-opt seccomp=` **replaces** the builtin rather than extending it, so
  ours would have to carry everything measured above — `default.json` is a
  generated artefact, ~830 lines, published under a moby tag and **absent from
  master**. A vendored copy that falls behind does not leave us standing still:
  Docker improves its builtin between releases and ours would not, so the sandbox
  would get quietly weaker than doing nothing. That is defensible only with a CI
  job that fails when the two diverge, and it is a cost worth paying for a
  different reason than this one — a Runner accepting problem types from outside
  our control, where the mount layout is no longer ours to promise.
- **`isolate` as a deeper supervisor.** **Not adopted** — the spike ran on
  2026-08-09 and is in `docs/spikes/ISOLATE.md`. It works in a non-privileged
  container, needing `CAP_SYS_ADMIN` and `CAP_NET_ADMIN` over a self-delegated
  cgroup v2 subtree. It was wanted for honest CPU-time and peak-memory numbers,
  and those turn out to be readable from the sandbox container's own cgroup with
  **nothing granted** — so the remaining trade was `CAP_SYS_ADMIN`, the
  capability that permits mounting, in a container that runs untrusted code.
  Reopening it needs a different argument than measurement.
- **Rootless Podman**, which is the more defensible posture for §3 and which the
  container client already speaks to.

## 7. Reporting something

This is a component that runs code written by people who are being marked. If
you find a way out of §2, or a way to reach the host that §3 does not describe,
say so before it is interesting: `AlgoJudge-Runner` issues, or privately to the
maintainers listed in `AUTHORS.txt`.
