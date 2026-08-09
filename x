#!/usr/bin/env sh
#
# cargo, in a pinned container.
#
# Rust is not a prerequisite for working on this repository. The Runner targets
# `linux/amd64` with cgroup v2 and a container runtime socket, so a native build
# on a developer's machine would exercise the parts that do not matter and skip
# the parts that do. Building where it runs is the cheaper habit.
#
# The image is pinned **by digest**, not by tag: a tag is a moving name and two
# people running `./x build` a month apart would compile against two compilers
# without either of them being told.
#
#   rust:slim  →  rustc 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1
#   read from the image itself on 2026-08-08
#
# Usage:
#   ./x build            ./x test            ./x fmt
#   ./x clippy           ./x run -- --help   ./x shell
#
set -eu

IMAGE='algojudge-runner-toolchain:1.97.1'

# Named volumes rather than directories in the working tree. The registry is
# worth keeping between runs — without it every build re-downloads the index —
# and `target/` in a volume means a container running as root never leaves
# root-owned files behind on a Linux host.
CARGO_VOLUME='algojudge-runner-cargo'
TARGET_VOLUME='algojudge-runner-target'

# Git Bash rewrites anything that looks like a path, which turns `/work` into a
# Windows directory and the bind mount into nonsense. `cygpath` gives Docker the
# host path in the form it expects.
if command -v cygpath >/dev/null 2>&1; then
    HOST_DIR="$(cygpath -w "$(pwd)")"
    export MSYS_NO_PATHCONV=1
else
    HOST_DIR="$(pwd)"
fi

# A terminal only when there is one. `-it` against a pipe or a CI log fails
# outright rather than degrading, which would make this script unusable from
# anything that is not a human at a prompt.
if [ -t 0 ] && [ -t 1 ]; then
    TTY='-it'
else
    TTY=''
fi

# Cached after the first run, so this costs a fraction of a second.
docker build -q -t "$IMAGE" -f Dockerfile.toolchain . >/dev/null

# Joining the stack's own network is how the conformance suite reaches a Server
# that publishes no port to the outside — which is the case in CI, and is the
# arrangement a real deployment has anyway.
#
# `container:<name>` is also accepted, and one test needs it: the maintenance
# switch answers only to a caller on the **Server's own loopback interface**, so
# sharing that container's network namespace is the only way a test can be one
# without a shell inside it. `--add-host` is dropped in that mode because the
# daemon refuses the two together, and it means nothing there anyway — the
# namespace is somebody else's.
HOST_ALIAS='--add-host=host.docker.internal:host-gateway'
if [ -n "${AJ_DOCKER_NETWORK:-}" ]; then
    NETWORK="--network=$AJ_DOCKER_NETWORK"
    case "$AJ_DOCKER_NETWORK" in
        container:*) HOST_ALIAS='' ;;
    esac
else
    NETWORK=''
fi

# The sandbox tests start sibling containers, so they need the runtime socket —
# the same thing the Runner itself holds in production.
#
# Opt-in, and worth being clear about what it costs: **anything that can reach
# this socket is root on the host.** Mounting it read-only would not change that;
# the flag applies to the socket file, not to the API spoken over it. It is here
# because running the isolation tests requires being the component that starts
# containers, and it is off by default so an ordinary `./x test` does not quietly
# take that privilege.
if [ -n "${AJ_DOCKER_SOCKET:-}" ]; then
    SOCKET='-v /var/run/docker.sock:/var/run/docker.sock'
    # And the cgroup hierarchy, **writable**, which is how peak memory is
    # measured: the Runner makes a cgroup, starts the sandbox under it, and
    # reads the parent after the child is gone — a container's own cgroup does
    # not outlive it, and the runtime API reports no peak on v2.
    #
    # `--cgroupns=host` matters as much as the mount. Without it this process
    # sees its own cgroup as the root, and the path it creates is not the path
    # the daemon resolves `--cgroup-parent` against — so the measurement would
    # read an empty directory rather than fail.
    #
    # Far cheaper than it looks: creating a directory in a mounted cgroup2 needs
    # write permission, not a capability.
    CGROUP='--cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup'
else
    SOCKET=''
    CGROUP=''
fi

run() {
    # shellcheck disable=SC2086
    docker run --rm $TTY $NETWORK $SOCKET $CGROUP \
        -v "$HOST_DIR:/work" \
        -v "$CARGO_VOLUME:/cargo" \
        -v "$TARGET_VOLUME:/work/target" \
        -w /work \
        -e CARGO_HOME=/cargo \
        -e CARGO_TERM_COLOR=always \
        -e AJ_TEST_SERVER \
        -e AJ_ADMIN_TOKEN \
        -e AJ_SANDBOX_ALLOW_CGROUP_V1 \
        -e RUST_LOG \
        -e "AJ_HOST_WORKDIR=$HOST_DIR" \
        $HOST_ALIAS \
        "$IMAGE" "$@"
}

case "${1:-}" in
    shell)
        run bash
        ;;
    clippy)
        shift
        # Every target, and a warning is a failure. Clippy that only advises is
        # clippy nobody runs.
        run cargo clippy --workspace --all-targets "$@" -- -D warnings
        ;;
    fmt)
        shift
        run cargo fmt --all "$@"
        ;;
    gate)
        # What CI runs, in the order that fails cheapest first.
        run cargo fmt --all --check
        run cargo clippy --workspace --all-targets -- -D warnings
        run cargo build --workspace --release
        run cargo test --workspace
        ;;
    '')
        echo 'usage: ./x <cargo subcommand> | fmt | clippy | gate | shell' >&2
        exit 2
        ;;
    *)
        run cargo "$@"
        ;;
esac
