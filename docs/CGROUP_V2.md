# cgroup v2 is required

> What the Runner needs from a host's control groups, why the requirement is
> hard, and how to tell whether a machine satisfies it before it is asked to
> mark anybody's work.
>
> Measured 2026-08-09 on the development host described in
> `../../docs/DEVELOPMENT_HOST.md`. `docs/SECURITY.md` §5 states the security
> side of the same requirement; this document is the operational one.

---

## 1. The one check that matters

The Runner asks the **container runtime**, not the filesystem:

```
docker info --format '{{.CgroupVersion}}'      # must print 2
```

`crates/aj-sandbox/src/docker.rs` reads `info.cgroup_version`, matches
`SystemInfoCgroupVersionEnum::_2`, and refuses to start on anything else.
Asking the daemon rather than reading `/sys/fs/cgroup` is deliberate: the Runner
may itself be in a container, and what *it* sees is a different question from
what the containers *it starts* will get.

*This cited `:211` until 2026-08-30 and `:334` until 2026-08-31, each right when
written and drifted within the day. **The number is gone rather than re-pinned a
third time** — search for `cgroup_version`. A line number is the citation most
likely to be wrong, and this document said so while carrying one.*

**Do not test with `[ -s /sys/fs/cgroup/cgroup.controllers ]`.** A cgroup
pseudo-file reports `st_size` zero however much it holds, so the size test calls
every host v1. Test the content.

## 2. Minimum requirements

| | Required | Why, and how to check |
|---|---|---|
| **Kernel** | cgroup v2 compiled in | `grep cgroup2 /proc/filesystems` |
| **Kernel** | **≥ 5.19** for peak memory | `memory.peak` arrived in 5.19; without it the peak has to come from `getrusage`, which is one process's resident set rather than the peak of everything the submission started |
| **Hierarchy** | **unified, not hybrid** | `cat /sys/fs/cgroup/cgroup.controllers` must be non-empty. A controller lives in **exactly one** hierarchy: if v1 holds it, a v2 mount is real and carries nothing |
| **Controllers** | `cpu`, `cpuset`, `memory`, `pids` | one per limit the sandbox sets — `--cpus`, `--cpuset-cpus`, `--memory`, `--pids-limit`. A missing controller does not error; the limit is simply not applied |
| **Delegation** | the controllers switched on for children | `cat /sys/fs/cgroup/cgroup.subtree_control` must list them. A controller present at the root and absent from `subtree_control` reaches no container |
| **Docker** | ≥ 20.10 | earlier versions have no cgroup v2 support at all |
| **Swap** | accounted, or absent | see §4 |

### Distributions

Unified is the default on **Ubuntu 21.10+**, **Debian 11+**, **Fedora 31+** and
**RHEL 9+**. An older host boots hybrid and needs the kernel parameter
`systemd.unified_cgroup_hierarchy=1`, then a reboot — this is not a runtime
switch, which is why "just enable v2" is a maintenance window rather than a
command.

### Docker Desktop on Windows

The cgroup hierarchy is mounted by the init of Docker Desktop's **own** WSL
distribution. Updating the user's Ubuntu changes nothing; `wsl --update` moves
the kernel and Docker Desktop decides the mount. Both were needed on this
workstation to get from v1 to v2.

## 3. What was verified on a conforming host

Kernel `6.18.33.2`, Docker `29.6.2`, cgroup v2, driver `cgroupfs`.

Root of the hierarchy — every controller present **and delegated**:

```
cgroup.controllers     : [cpuset cpu io memory hugetlb pids rdma]
cgroup.subtree_control : [cpuset cpu io memory hugetlb pids rdma]
```

A sandbox container under the **full production profile** —
`--cap-drop=ALL --security-opt=no-new-privileges --user 65534:65534`, nothing
granted — sees every interface the Runner needs, and the limits are the ones it
was given:

```
memory.peak  memory.max  memory.events  cpu.stat  pids.max  cpuset.cpus
memory.max = 67108864   (--memory=64m)
pids.max   = 32         (--pids-limit=32)
```

**The limit is enforced, and the measurement is honest.** A program told to
allocate past a 32 MiB limit:

```
memory.events: oom 1, oom_kill 1
memory.peak  = 33554432        exactly the limit
```

Both suites passed on this host with **no escape hatch**, on 2026-08-09:
`aj-sandbox --test adversarial` **11/11**, `aj-standard-io --test judging`
**10/10**.

> **Both suites have grown since, so those fractions describe 2026-08-09 and
> nothing later.** Counted on 2026-08-30: `crates/aj-sandbox/tests/adversarial.rs`
> held **12** cases and `crates/aj-standard-io/tests/judging.rs` held **21**; on
> 2026-08-31 they hold **16** and **22**. That is a count of the files, not a
> run — this document reports one measurement on one host and a fresh number
> belongs to a fresh measurement.
>
> **This said "none of them ignored", and the truth is the opposite: every one
> is.** Both suites need a container runtime, so every case carries `#[ignore]`
> and `--include-ignored` is mandatory — which is why the commands below carry
> it. A reader who believed the old clause ran them without it and saw
> `0 passed; 0 failed; 16 ignored`, which reads like a pass.
>
> **And neither command above is runnable as written.** Rust is not installed on
> the development host; everything goes through the container wrapper:
>
> ```
> AJ_DOCKER_SOCKET=1 ./x test -p aj-sandbox --test adversarial -- --include-ignored --test-threads=1
> AJ_DOCKER_SOCKET=1 ./x test -p aj-standard-io --test judging -- --include-ignored --test-threads=1
> ```
>
> Both suites need the socket, and this named only the adversarial one until
> 2026-08-31 — so the judging command was printed without it and could not have
> worked. `AJ_DOCKER_SOCKET` is what mounts the runtime socket and
> `/sys/fs/cgroup`; both start sibling containers, and the adversarial one also
> reads their cgroups. An ordinary `./x test` deliberately does not take that
> privilege — anything that can reach the socket is root on the host.
>
> `--test-threads=1` is load-bearing too: run in parallel these fight over the
> container runtime and all of them fail. And **the judging suite needs the four
> language images built first** — nothing builds them for you, and every case
> fails at its first line without them.

## 4. Swap, which is easy to get wrong in both directions

`--memory` without `--memory-swap` equal to it means the process swaps instead
of being killed, and the limit means nothing. That is why the sandbox sets both.

On cgroup v2 this lands as `memory.swap.max`, and it **works** — verified:

```
docker run --memory=64m --memory-swap=64m …
  memory.max      = 67108864
  memory.swap.max = 0            no swapping at all
```

**One trap, worth naming because it produced a wrong conclusion here first.**
The **root** cgroup of a v2 hierarchy does not expose controller interface files
at all — no `memory.max`, no `memory.swap.max`. Reading the root and finding
`memory.swap.max` missing looks exactly like "this kernel has no swap
accounting", and it is not: the file appears in every non-root cgroup, which is
where it matters. Check a container, not the root.

`isolate-check-environment` reports swap accounting as absent on this host. The
container evidence above contradicts it, so that check appears to be looking for
the v1 interface.

**For an evaluation host the recommendation stands anyway: turn swap off.**
Upstream `isolate` says the same. Swapped-out pages make a timing measurement a
property of the machine's memory pressure rather than of the program.

## 5. What the number actually covers

The limit and the reading are the **container's cgroup**, not the process. So
the question "is this the program, or is there overhead?" has a measured answer,
and it matters because calibration turns these numbers into limits somebody's
submission has to meet.

Measured with a static binary that allocates a stated amount and then reads its
own `memory.peak`, under the full sandbox profile:

| allocated | peak | over |
|---|---|---|
| 0 MiB | 5.43 MiB | 5.43 |
| 16 MiB | 20.88 MiB | 4.88 |
| 64 MiB | 66.11 MiB | 2.11 |
| 128 MiB | 129.92 MiB | 1.92 |

**The overhead shrinks as the program grows**, which is the signature of
reclaimable page cache: about **2 MiB is irreducible** and the rest is cache the
kernel drops under pressure rather than adding on top. It does not scale, so it
never turns into a proportional error.

Four things that cost **nothing** measurable:

- **The shell.** The run command is `exec …`, so the shell is *replaced* rather
  than kept — 5.34 MiB through `sh -c 'exec …'` against 5.43 MiB direct.
- **Redirecting and reading input.** A 32 MiB input drained in full moved
  nothing: 65.42 MiB against 66.69 MiB without it, which is inside the noise.
- **Repeat runs.** Five identical runs spanned 65.44–66.31 MiB, so the
  measurement is good to about **±0.5 MiB**.
- **The language.** A trivial C++ solution peaks at 6.68 MiB and a trivial
  Python one at 6.98 MiB against a 5.5 MiB container floor — the interpreter
  costs about **0.3 MiB more** than a compiled binary at startup. Far too small
  to be why a Python solution fails a memory limit.

One thing that costs **exactly what it writes**:

- **The scratch tmpfs.** Writing 8 / 32 / 64 MiB gave peaks of 14.92 / 34.79 /
  67.08 MiB. tmpfs pages are charged to the cgroup and are **not reclaimable**,
  so scratch space is memory. Going past the limit that way is an ordinary
  memory kill, verified: `dd` returned **137** and the daemon reported
  `OOMKilled=true`.

And one that is much larger than the run it belongs to:

- **Compilation.** `g++ -O2` on a single file that includes `<iostream>` peaks
  at **45.26 MiB** — roughly seven times the floor, and far above anything the
  compiled program then uses. The compile profile's own memory limit is what
  that has to fit inside; it is not a property of the submission.

### How the number is actually taken, and what it costs to deploy

Two facts make the obvious approaches impossible, both measured on 2026-08-09:

- **The runtime API reports no peak on cgroup v2.** `memory_stats` carries
  `limit`, `usage` and the contents of `memory.stat`, and none of those is a
  maximum. CPU time *is* there and agrees with `cpu.stat`.
- **A container's own cgroup is destroyed when it exits.** After `docker wait`
  the directory is already gone, so there is no window to read it in.

So the Runner **makes a cgroup of its own**, starts the sandbox under it with
`--cgroup-parent`, and reads the parent once the child is gone. The parent
survives because it belongs to the Runner; one container per test and a fresh
parent per run mean the parent's peak is that program's peak.

**What this asks of a deployment**: the Runner needs the cgroup hierarchy
mounted **writable**, and — when the Runner is itself in a container —
`--cgroupns=host`, so that the path it creates is the path the daemon resolves
`--cgroup-parent` against. Without the host namespace the Runner sees its own
cgroup as the root and would read an empty directory rather than fail.

```
--cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup
AJ_Sandbox__CgroupRoot      # defaults to /sys/fs/cgroup
```

**And a third requirement, unstated here until 2026-08-31: the daemon's cgroup
driver must be `cgroupfs`.** With `systemd`, the default on RHEL 9+, Fedora and
Ubuntu, a cgroup parent is not a path at all.

**All three are refusals since 2026-09-02, not degradations.** This used to give
up with one `info` line and judge anyway, so an operator could satisfy every row
of the table above, mount the hierarchy writable, pass `--cgroupns=host`, and
still get nothing — with a single line to say so. That was defensible while the
reading was only printed beside a verdict. A time limit is now decided on
processor time read from `cpu.stat`, so a Runner that cannot read one cannot
judge, and says so at start rather than failing every job it later claims.

```
docker info --format '{{.CgroupDriver}}'       # must print cgroupfs
```

This is how it was found, and the code records it: CI caught it and the
workstation did not, because the workstation runs `cgroupfs` and the runner runs
`systemd`, so every container in the adversarial suite failed to start on a host
the author never used.

**It costs write permission, not a capability.** Creating a directory in a
mounted cgroup2 needs neither `CAP_SYS_ADMIN` nor privilege — which is the whole
reason D-13 could decline `isolate` and keep the numbers.

Where the mount is absent the Runner **refuses to start**. It was a supported
configuration until 2026-09-02, when the reading stopped being a number beside a
verdict and became the thing the verdict is made of.

### The rule this implies for calibration

**Do not subtract the overhead.** A participant's submission runs in a container
of the same shape as the model solution's, so the floor is present in both
measurements and cancels. Correcting for it would make every limit about 2 MiB
too tight, for the sake of a number nobody meets.

`PACKAGE_FORMAT.md` defaults memory to `measured + 16 MiB` rather than a
multiple. Against a 2 MiB floor, a ±0.5 MiB spread and a 0.3 MiB gap between
languages, that headroom is comfortable — which is now a measurement rather than
a guess.

## 6. What v1 does and does not do

Stated so the requirement is not read as stricter than it is.

**v1 enforces.** Measured on this host before the upgrade: a container over its
memory limit is OOM-killed and `OOMKilled` is reported, and the whole adversarial
suite passed. The sandbox holds on v1.

**v1 cannot measure.** `memory.peak` and `cpu.stat` are v2 interfaces. The
refusal is about reaching a verdict at all — a time limit is processor time —
and not about whether the sandbox contains a program. It contains one either
way.

`AJ_Sandbox__AllowUnmeasured` starts anyway — and only starts; such a Runner
fails every job it claims. It is off by default, shouts at `ERROR` on every start, and is
**consulted only when preflight fails** — so a CI job that merely brings the
stack up would pass while measuring nothing. That is why CI asserts the version
itself before anything else runs.
