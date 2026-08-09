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
| read-only root filesystem | writes go to a tmpfs or nowhere |
| tmpfs `rw,noexec,nosuid` | scratch that cannot be executed from |
| `--user 65534:65534` | never root, even inside |
| `--memory` **with `--memory-swap` equal** | without the second the limit means nothing: the process swaps instead of being killed |
| `--pids-limit` | a fork bomb hits a wall |
| `--cpus` **and `--cpuset-cpus`** | capping CPU is not pinning it; without a pinned core, threads buy wall-clock time the single-thread rule does not give them |
| wall clock = the problem's limit **plus one second** | something outside the process has to reap one stuck in an uninterruptible syscall |
| an output cap enforced **while it runs** | read afterwards, a flooding program fills the host's disk first |

Every row has a test in `crates/aj-sandbox/tests/adversarial.rs`, and each
asserts two things: the program was stopped correctly, **and the host is
unchanged afterwards**. A sandbox that contains a program by leaking a process
has not contained it.

Two more, from the pipeline rather than the sandbox:

- **The build gets no writable host path.** It writes to its own container layer
  and the artefact is read back through the runtime API. The alternative was
  opening a directory to every user on the host.
- **A package-supplied checker is sandboxed too.** It comes from a problem
  author rather than from the platform, and it runs with its own limits and no
  network, never in the Runner's process.

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

Checked at start; the Runner refuses without it.

**The limits are enforced on v1** — measured, not assumed: a container over its
memory limit is OOM-killed there and `OOMKilled` is reported. What v1 cannot do
is **measure honestly**: `memory.peak` and `cpu.stat` are v2 interfaces, and
`isolate` 2.x dropped v1 outright. Those numbers are shown to a participant
beside their verdict, and a number that is sometimes wrong is worse than no
number.

`AJ_Sandbox__AllowCgroupV1` starts anyway, for a development machine whose
Docker still reports v1. It is off by default and shouts at `ERROR` on every
start, because a quiet override is a production setting waiting to happen.

## 6. What is not here yet

Stated so that absence is not read as a decision:

- **A custom seccomp profile.** Docker's default profile applies, which already
  blocks a large set of syscalls; a hand-written one is on the roadmap and is
  easy to get wrong in the direction of breaking a legitimate runtime.
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
