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
if [ -n "${AJ_DOCKER_NETWORK:-}" ]; then
    NETWORK="--network=$AJ_DOCKER_NETWORK"
else
    NETWORK=''
fi

run() {
    # shellcheck disable=SC2086
    docker run --rm $TTY $NETWORK \
        -v "$HOST_DIR:/work" \
        -v "$CARGO_VOLUME:/cargo" \
        -v "$TARGET_VOLUME:/work/target" \
        -w /work \
        -e CARGO_HOME=/cargo \
        -e CARGO_TERM_COLOR=always \
        -e AJ_TEST_SERVER \
        -e RUST_LOG \
        --add-host=host.docker.internal:host-gateway \
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
