# Spike: `isolate` as the innermost supervisor

> D-6 accepted `isolate` 2.x **provisionally**, pending a spike on cgroup
> delegation and the capabilities it requires inside a non-privileged container.
> This is that spike's result so far. Run on 2026-08-09.
>
> Harness: `spikes/isolate/`. Every line below came from running something; the
> probe is committed so the numbers can be disputed by re-running them rather
> than by argument.

Legend: **VF** verified by running · **UP** upstream's own statement, cited ·
**OQ** open · **AI** architectural tension.

---

## 1. The short version

**Two of the three questions are answered, and the answer to the capability
question is better than upstream leads you to expect.** `isolate` runs a program
in a **non-privileged** container with exactly two added capabilities. It does
not need `--privileged`.

**The cgroup question is not answered**, because the development host cannot
answer it: it runs cgroup v1 and `isolate` 2.x requires v2. That is a property
of the host, not a finding about `isolate`, and it is stated here rather than
guessed at.

## 2. What was verified by running

| | Result |
|---|---|
| **VF** `isolate --run`, container with `--cap-drop=ALL --security-opt=no-new-privileges --user 65534` | **`Must be started as root`** — `no-new-privileges` voids the setuid bit, so `isolate` cannot become root even though the binary is setuid |
| **VF** default Docker capabilities (no `CAP_SYS_ADMIN`) | **`Cannot run proxy, clone failed: Operation not permitted`** — the namespace `clone()` is refused |
| **VF** `--cap-add=SYS_ADMIN` | further, then **`SIOCSIFFLAGS on 'lo' failed`** — bringing up loopback in the new netns needs more |
| **VF** `--cap-add=SYS_ADMIN --cap-add=NET_ADMIN` | **works**: `hello from the box OK (0.003 sec real, 0.003 sec wall)` |
| **VF** `--privileged` | same result, no better. **Privilege buys nothing here** |

**VF — the minimum capability set for the non-cgroup path is `CAP_SYS_ADMIN` +
`CAP_NET_ADMIN`.** Found by walking the ladder, each rung named by the error it
produced rather than by reading a manual.

That `CAP_SYS_ADMIN` is required is also what the source says:
`rules.c:367` — `cap_value_t cap_list[] = { CAP_SYS_ADMIN };`, and the comment at
`rules.c:474` gives the reason: *"needed for mount"*.

## 3. Why the cgroup question could not be answered here

**VF** — the host is cgroup **v1**. `docker info` reports `CgroupVersion: 1`;
kernel `5.15.133.1-microsoft-standard-WSL2`; Docker `24.0.7`.

The kernel is not the obstacle in itself — `/proc/filesystems` lists `cgroup2`
and mounting it succeeds. The obstacle is that **a controller lives in exactly
one hierarchy at a time**, and all fourteen are on v1:

```
cpuset hierarchy=1   cpu hierarchy=2   memory hierarchy=5   pids hierarchy=12  …
```

So a `cgroup2` mount here is real and **empty**: `cgroup.controllers` is
**0 bytes**. `isolate` then gets as far as creating its group and fails where the
first controller file is read:

```
isolate --cg --init   rc=0
isolate --cg --run    rc=2  hello from the box
                            Cannot open /cg2/isolate/box-0/memory.events: No such file or directory
```

The program *ran* — the failure is measurement, not containment.

**One trap worth naming.** The same run reports `max-rss:1572`, and it would be
easy to read that as peak memory working. It is not: `max-rss` comes from
`getrusage()`, not from a controller — it is one process's resident set, not the
peak of everything the submission started. The number D-6 was accepted for is the
cgroup one, and that one is **absent**.

## 4. What upstream says, cited

- **UP** `NEWS:123` — *"This version runs only on systems supporting CGroup v2 …
  If you need to stick with CGroup v1, please use Isolate 1.10.1."*
- **UP** `isolate.1.txt` — *"Reporting memory usage requires Linux kernel 5.19 or
  newer."* This host runs **5.15**, so even on a v2 host this kernel would not
  give the number.
- **UP** `isolate.1.txt:406` — *"Running Isolate in containers is not
  recommended, since container managers usually do not delegate control groups
  properly. Besides, you do not want to share the machine with other workloads,
  which would influence measurement of execution time. If you still want to use
  containers, you are on your own and you probably have to make them
  privileged."*

That last sentence is **contradicted by §2 for the non-cgroup path**: two
capabilities were enough and privilege changed nothing. Whether it holds for the
cgroup path is exactly what is still open.

- **UP** `isolate-check-environment` on this host wants swap off, SMT off, ASLR
  off and transparent hugepages off, and warns that **without swap accounting
  `isolate` cannot enforce memory limits**. These are not container settings.
  They describe a **dedicated, tuned evaluation host** — which is the same
  conclusion `docs/SECURITY.md` §1 reaches from the other direction.

## 5. The tension this creates with D-5

**AI** — D-5 rejects privileged containers because *the privilege the
infrastructure needs is the privilege an escapee inherits*. `CAP_SYS_ADMIN` is
not `--privileged`, but it is the capability that allows mounting, and a
container that has it is widely treated as one escape away from the host.

So the honest shape of the trade is: **`isolate` would be an inner boundary
bought by weakening the outer one.** Whether that is worth it depends on a
number this spike has not yet produced — how much better the measurement
actually is — which is why the remaining work is worth doing before deciding.

Two deployments avoid the trade entirely and should be compared against it:
`isolate` on the **host** with the Runner as a host process, which is how
upstream ships it (a systemd unit delegating `isolate.scope`); or the current
container-only pipeline, which measures wall clock honestly and peak memory not
at all.

## 6. What is still open

**OQ 1 — does `isolate --cg` work in a non-privileged container on a cgroup v2
host?** Needs `--cgroupns=private` and a writable delegated subtree. The
capability set in §2 may grow; that is the point of running it.

**OQ 2 — are the numbers better?** Peak memory and CPU time from `isolate`
against what the container pipeline reports today, on the same programs. D-6 was
accepted for this and it has never been measured.

**OQ 3 — does `CAP_SYS_ADMIN` survive review** against D-5, given the answer to
OQ 2.

## 7. How to finish it

The probe is the deliverable, not a description of one:

```
docker build -t aj-isolate-spike spikes/isolate
docker run --rm --cap-add=SYS_ADMIN --cap-add=NET_ADMIN aj-isolate-spike
```

It prints the host's cgroup version first, so a run on the wrong host is obvious
rather than misleading. On a v2 host it takes the delegated-subtree path by
itself and reports the three numbers that matter — time, wall time and peak
memory — or says which is absent.

**Before re-running, the host needs**: cgroup v2 (`docker info` must report `2`)
and kernel **≥ 5.19** for the memory number. On this workstation that means
updating WSL and Docker Desktop; `wsl --update` moves the kernel, and it is
Docker Desktop's own distribution — not the user's Ubuntu — that decides how the
cgroup hierarchies are mounted.

**Fallback, per the plan.** If delegation turns out to need privileges the
container must not have, the container path stays and this document says why.
That is a result, not a failure — and §2 has already narrowed what "privileges"
would mean.
