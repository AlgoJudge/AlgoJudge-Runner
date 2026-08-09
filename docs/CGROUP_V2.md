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

`crates/aj-sandbox/src/docker.rs:211` reads `info.cgroup_version` and refuses to
start on anything else. Asking the daemon rather than reading `/sys/fs/cgroup`
is deliberate: the Runner may itself be in a container, and what *it* sees is a
different question from what the containers *it starts* will get.

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

Both suites pass on this host with **no escape hatch**:
`aj-sandbox --test adversarial` **11/11**, `aj-standard-io --test judging`
**10/10**.

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

## 5. What v1 does and does not do

Stated so the requirement is not read as stricter than it is.

**v1 enforces.** Measured on this host before the upgrade: a container over its
memory limit is OOM-killed and `OOMKilled` is reported, and the whole adversarial
suite passed. The sandbox holds on v1.

**v1 cannot measure.** `memory.peak` and `cpu.stat` are v2 interfaces. The
refusal is about the number shown to a participant beside their verdict, not
about whether the sandbox contains them.

`AJ_Sandbox__AllowCgroupV1` starts anyway, for a development machine whose Docker
still reports v1. It is off by default, shouts at `ERROR` on every start, and is
**consulted only when preflight fails** — so a CI job that merely brings the
stack up would pass while measuring nothing. That is why CI asserts the version
itself before anything else runs.
