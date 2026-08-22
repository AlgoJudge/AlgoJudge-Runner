# The GNU toolchain a C or C++ submission is built and run with.
#
# Ours rather than a stock compiler image, for two reasons the analysis names:
# language images became this project's responsibility when Judge0 left the MVP
# (D-3), and the compiler version is **reported as a capability**, so it has to
# be a thing we pin rather than a thing that moves under us.
#
# ── Debian, not Alpine (2026-08-22) ─────────────────────────────────────────
#
# This was `alpine:3`, on the argument that the run container is started once
# per test and image size is start-up latency. That argument was real and it
# lost to two things the language catalogue needs:
#
#   - **PyPy has no official musl build**, so `images/pypy` could not exist on
#     Alpine at all; and
#   - **Clang plus static linking is less certain on musl than on glibc**, and
#     half the catalogue is Clang.
#
# Both disappear together on Debian, and a toolchain set that behaves the same
# way across its eighteen entries is worth more than the start-up time. Every
# language image goes the same way, so nothing is judged on two libcs.
#
# Trixie rather than bookworm, and not for freshness: **`-std=c23` and
# `-std=c++23` are what the catalogue asks for**, and bookworm's GCC 12 knows
# them only as `-std=c2x` and `-std=c++2b`. A base that cannot spell the flag
# turns four catalogue rows into compilation errors for every submission.
#
# Two consequences of that move, recorded rather than discovered later:
#
#   - **`-static` stays**, and its old justification does not: a test is started
#     from this same image, so the shared libraries are present and a dynamic
#     binary would run. It is kept because it makes the artefact independent of
#     whatever image it is run in, and keeps the loader out of the measured
#     time. See `language.rs`, which carries the whole of that.
#   - **glibc's static linking has a known caveat**: `getaddrinfo` and
#     `gethostbyname` do not work in a fully static binary, and the linker warns
#     about it on every build. Harmless here — network access is disabled for
#     judged code by default (`.claude/rules/runner.md`) — and written down so it
#     is an accepted trade rather than a warning somebody later mistakes for a
#     broken image.

FROM debian:trixie-slim

# `libc6-dev` for `libc.a` and `libstdc++-14-dev` for `libstdc++.a` — the static
# halves `-static` links against. Both arrive as dependencies of `g++` anyway;
# they are named here so that a base image which stops pulling them in fails at
# `docker build` rather than at somebody's submission.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc g++ libc6-dev libstdc++-14-dev \
 && rm -rf /var/lib/apt/lists/*

# Reported to the Server as this Runner's capability, and written into the
# result's log so a verdict can be traced to the compiler that produced it.
RUN { gcc --version | head -1; g++ --version | head -1; } > /etc/algojudge-toolchain

# Where a build puts what it made. Part of the image on purpose: the container
# runs as an unprivileged user and cannot create a directory at the root of its
# own filesystem, so this is the one place the contract has to be declared
# rather than assumed. `aj-standard-io`'s `BUILD_OUTPUT` names the same path.
RUN mkdir -p /out && chown 65534:65534 /out

# Not root, and not able to become root. The sandbox drops every capability
# anyway; this is the second lock on the same door.
#
# No `adduser` line: Debian ships `nobody:nogroup` at 65534:65534 already, and
# the Alpine one here was BusyBox syntax that Debian's Perl `adduser` rejects.
USER 65534:65534
