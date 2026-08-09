#!/bin/sh
# Runs inside the container. One question per section, each answered by running
# something rather than by reading a manual.
#
# Prints a line per finding. The caller decides what is a failure, because the
# same probe is informative on a host that cannot support isolate at all.
set -u

say() { printf '%s\n' "$*"; }
kv()  { printf '  %-34s %s\n' "$1" "$2"; }

say "== host =="
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
    kv "cgroup seen by this container" "v2 unified"
    kv "controllers" "[$(cat /sys/fs/cgroup/cgroup.controllers)]"
else
    kv "cgroup seen by this container" "v1 legacy (or no controllers on v2)"
fi
kv "kernel" "$(uname -r)"
# Peak memory is a v2 interface and upstream additionally requires 5.19+ for it,
# so the kernel is reported rather than assumed from the cgroup version alone.
say ""

say "== isolate =="
kv "version" "$(isolate --version 2>&1 | head -1)"
say ""

# A sandbox that never runs anything is not evidence, so every probe below runs
# a real program and reports what came back.
#
# The mode is passed in, because a box initialized without `--cg` and then run
# with it fails as "incompatible control group mode" — a true message about the
# wrong thing, which reads exactly like the answer this spike is looking for.
prepare() {
    isolate ${1:-} --cleanup >/dev/null 2>&1
    isolate ${1:-} --init >/dev/null 2>&1 || return 1
    echo "hello from the box" > /var/local/lib/isolate/0/box/t.txt
}

say "== can isolate run a program at all =="
if prepare; then
    out=$(isolate --run -- /bin/cat t.txt 2>&1); rc=$?
    kv "isolate --run" "rc=$rc  $(echo "$out" | tr '\n' ' ')"
else
    kv "isolate --init" "failed: $(isolate --init 2>&1 | head -1)"
fi
say ""

say "== control-group mode =="
# The question the spike exists for.
#
# A mounted cgroup2 is not a delegated one. Two things have to happen that
# systemd's `Delegate=yes` does for the packaged install, and neither is
# obvious from a failure: the controllers must be switched on for children via
# `cgroup.subtree_control`, and that write is **refused while the cgroup still
# holds processes** — silently, leaving the file empty rather than erroring. So
# the container's own processes move into a sibling first.
CG_ROOT=""
mkdir -p /cg2
if mount -t cgroup2 none /cg2 2>/dev/null; then
    kv "mounted cgroup2 at /cg2" "controllers=[$(cat /cg2/cgroup.controllers)]"

    # Empty here means a v1 host: the mount is real, and every controller lives
    # in exactly one hierarchy, which v1 already holds.
    #
    # Tested by content, not by `[ -s ]`: a cgroup pseudo-file reports st_size
    # zero however much it holds, so the size test calls every host v1.
    if [ -n "$(cat /cg2/cgroup.controllers)" ]; then
        mkdir -p /cg2/init
        while read -r pid; do echo "$pid" > /cg2/init/cgroup.procs 2>/dev/null || true; done < /cg2/cgroup.procs
        kv "processes left in the root" "$(wc -l < /cg2/cgroup.procs)  (must be 0 to delegate)"

        echo "+cpuset +cpu +memory +pids" > /cg2/cgroup.subtree_control 2>/dev/null || true
        kv "root subtree_control" "[$(cat /cg2/cgroup.subtree_control)]"

        mkdir -p /cg2/isolate
        echo "+cpuset +cpu +memory +pids" > /cg2/isolate/cgroup.subtree_control 2>/dev/null || true
        kv "delegated to isolate/" "[$(cat /cg2/isolate/cgroup.controllers)]"
        CG_ROOT=/cg2/isolate
    fi
else
    kv "mount -t cgroup2" "refused"
fi

if [ -n "$CG_ROOT" ]; then
    kv "cg_root" "$CG_ROOT"
    sed -i "s|^cg_root = .*|cg_root = $CG_ROOT|" /usr/local/etc/isolate
    # Both modes, because the box left by the section above was initialized
    # without `--cg` and a mismatched cleanup leaves it in place — after which
    # every cgroup probe answers "incompatible control group mode" and the
    # spike concludes something about the wrong subject.
    isolate --cleanup >/dev/null 2>&1
    isolate --cg --cleanup >/dev/null 2>&1
    init_out=$(isolate --cg --init 2>&1); init_rc=$?
    kv "isolate --cg --init" "rc=$init_rc  $(echo "$init_out" | tr '\n' ' ')"
    if [ "$init_rc" -eq 0 ]; then
        echo "hello from the box" > /var/local/lib/isolate/0/box/t.txt
        out=$(isolate --cg --run --meta=/tmp/meta -- /bin/cat t.txt 2>&1); rc=$?
        kv "isolate --cg --run" "rc=$rc  $(echo "$out" | tr '\n' ' ')"
        if [ -f /tmp/meta ]; then
            kv "time reported" "$(grep -E '^time:' /tmp/meta || echo 'absent')"
            kv "wall time reported" "$(grep -E '^time-wall:' /tmp/meta || echo 'absent')"
            # The number D-6 was accepted for. Absent here means the whole
            # argument for isolate is unproven on this host.
            kv "peak memory reported" "$(grep -E '^max-rss:|^cg-mem:' /tmp/meta || echo 'absent')"
        fi
    fi
fi
say ""

say "== what upstream would want fixed on this host =="
isolate-check-environment 2>&1 | grep -E "WARNING|FAIL" | head -10
