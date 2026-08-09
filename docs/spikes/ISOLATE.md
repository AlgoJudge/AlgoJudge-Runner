# Spike: `isolate` as the innermost supervisor

> D-6 accepted `isolate` 2.x **provisionally**, pending a spike on cgroup
> delegation and the capabilities it requires inside a non-privileged container.
> This is the result. Run 2026-08-09 on cgroup v2, kernel `6.18.33.2`.
>
> Harness: `spikes/isolate/`. Every number below came from running something,
> and the probe is committed so they can be disputed by re-running rather than
> by argument.

Legend: **VF** verified by running · **UP** upstream's own words, cited ·
**R** recommendation.

---

## 1. The answer

**`isolate` works, and it is no longer worth adopting for the reason it was
accepted.**

Both halves are verified. It runs in a **non-privileged** container and reports
CPU time, wall time and cgroup peak memory. And **the container pipeline already
reports the same things, at zero privilege cost** — which is what D-6 said
`isolate` was for.

## 2. `isolate` in a non-privileged container — it works

**VF** — with `--cap-add=SYS_ADMIN --cap-add=NET_ADMIN` and a self-delegated
cgroup v2 subtree, on a program allocating 64 MiB and burning some CPU:

```
time:0.102   time-wall:0.121   cg-mem:66008   max-rss:66920   exitcode:0
```

`--privileged` was tried and changes nothing, so **UP** `isolate.1.txt:406` —
*"you probably have to make them privileged"* — is not what we measured.

**VF — the minimum capability set is `CAP_SYS_ADMIN` + `CAP_NET_ADMIN`**, found
by walking the ladder and letting each rung name itself:

| profile | what stopped it |
|---|---|
| our sandbox profile (`cap-drop=ALL`, `no-new-privileges`, `user 65534`) | `Must be started as root` — `no-new-privileges` voids the setuid bit |
| Docker defaults (no `CAP_SYS_ADMIN`) | `Cannot run proxy, clone failed: Operation not permitted` |
| `+SYS_ADMIN` | `SIOCSIFFLAGS on 'lo' failed` — loopback in the new netns |
| `+SYS_ADMIN +NET_ADMIN` | nothing; it runs |

`CAP_SYS_ADMIN` is also what the source asks for: `rules.c:367` —
`cap_value_t cap_list[] = { CAP_SYS_ADMIN };`, commented at `rules.c:474` as
*"needed for mount"*.

### Delegation is two steps, and the second fails silently

A mounted `cgroup2` is **not** a delegated one, and this is where the spike spent
most of its time. What systemd's `Delegate=yes` does for the packaged install has
to be done by hand:

1. controllers are switched on for children by writing `cgroup.subtree_control`;
2. **that write is refused while the cgroup still holds processes** — and it is
   refused *silently*, leaving the file empty rather than returning an error.

So the container's own processes are moved into a sibling cgroup first, and only
then does the subtree carry `memory.events`, `memory.max`, `memory.peak`,
`cpu.stat` and `cpuset.cpus`. Without step 2 `isolate` gets as far as creating
its box and fails on `Cannot open …/memory.events`, which reads like a missing
controller rather than a delegation that never happened.

One more trap, cheap to hit: **a cgroup pseudo-file reports `st_size` zero
however much it holds**, so `[ -s cgroup.controllers ]` calls every host v1. Test
the content.

## 3. Why it is not worth adopting for measurement

**VF** — the same workload, in a container under **our full production sandbox
profile** — `--cap-drop=ALL --security-opt=no-new-privileges --user 65534:65534`,
**no added capability at all**:

```
memory.peak = 69492736 B  (67 864 KiB)
cpu.stat: usage_usec 124786   user_usec 65859   system_usec 58926
```

That is the honest peak memory and the honest CPU time, read from the container's
own cgroup, with nothing granted. D-6's rationale was that `isolate` *"yields
correct CPU-time and peak-memory numbers instead of reimplementing accounting"*.
On a cgroup v2 host, so does the cgroup, and we are already on one.

The two measurements are of the same order and not identical — `isolate` measures
its box, the container figure includes the shell that started the program — so
they are comparable in kind, not to the millisecond.

### One wrinkle that decides how the Runner reads it

**VF — Docker's stats API does not expose peak memory on cgroup v2.**
`memory_stats` carries only `limit` and `usage`, and `usage` sampled after the
program finished read 1.7 MB against a real peak of 67.9 MiB. CPU time *is*
there: `cpu_usage.total_usage` 125 481 000 ns, agreeing with `cpu.stat`.

So peak memory has to come from the cgroup file, not the API. Two ways, and the
choice is a real one:

- **The container reads its own** `/sys/fs/cgroup/memory.peak` after the program
  exits — verified above, no race, no mount, but the reader sits beside untrusted
  code and its output has to be treated accordingly.
- **The Runner reads the sibling's cgroup from the host**, which needs a
  read-only mount of `/sys/fs/cgroup` into the Runner — far weaker than
  `CAP_SYS_ADMIN`, but it must happen before the container's cgroup is destroyed.

## 4. What upstream wants regardless

**UP** — `isolate-check-environment` on this host still asks for swap off, SMT
off, ASLR off and transparent hugepages off, and warns that **without swap
accounting memory limits are not enforced**. **UP** `isolate.1.txt` also warns
against sharing the machine with other workloads.

None of that is a container setting. It describes a dedicated, tuned evaluation
host — the same conclusion `docs/SECURITY.md` §1 reaches from the security side,
and it applies to the container pipeline just as much.

## 5. Recommendation

**R — close D-6 as "not adopted for the stated reason".** The measurement
argument is spent: the numbers are available without granting anything, and
`CAP_SYS_ADMIN` in a container that runs untrusted code is the capability that
permits mounting. Buying an inner boundary by weakening the outer one is a poor
trade when the thing it was bought for is free.

**R — take the number, not the tool.** Read `memory.peak` and `cpu.stat` from the
sandbox container's cgroup and report them. This unblocks memory calibration,
which `PACKAGE_FORMAT.md` records as waiting on exactly this measurement, and it
costs no capability.

**R — reopen `isolate` only under a different argument.** Defence in depth and
its syscall filtering are real, and neither has been measured here. That would be
a new decision with its own evidence, not this one continued.

**R — keep the preflight.** cgroup v2 stays required. Everything in §3 is a v2
interface, so the check that refuses to start on v1 is now load-bearing for
measurement, not only for `isolate`.
