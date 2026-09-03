# The CPython runtime a submission is checked and run with.
#
# There is no compilation step, but there **is** a build step: `py_compile`
# turns a syntax error into a `CompilationError` before any test runs. Without
# it every test fails identically with a traceback, and a participant with a
# missing colon is told their answer is wrong on all of them.
#
# `python:3-slim` is Debian-based, which is the base every language image moved
# to on 2026-08-22 — see `images/gcc/Dockerfile` for why. It was
# `python:3-alpine`.

# ── the measuring shim ──────────────────────────────────────────────────────
#
# **Built here rather than copied in from somewhere**, so an image is one
# `docker build` from its sources with nothing that has to be built first. The
# stage is identical in all four files and Docker's cache builds it once.
#
# The build context is `images/`, not `images/<name>/`, because that is what
# puts `shim/` inside it:
#
#   docker build -f images/gcc/Dockerfile -t algojudge/lang-gcc:local images/
#
# Static, so it depends on nothing in the image it lands in -- which is what
# lets one binary serve a Debian base and a PyPy one alike. `-Werror` because a
# warning in this program is a warning in the thing every measured verdict rests
# on.
FROM debian:trixie-slim AS shim
RUN apt-get update  && apt-get install -y --no-install-recommends gcc libc6-dev  && rm -rf /var/lib/apt/lists/*
COPY shim/aj-shim.c /tmp/aj-shim.c
RUN gcc -O2 -Wall -Wextra -Werror -static -o /aj-shim /tmp/aj-shim.c

FROM python:3-slim

RUN python3 --version > /etc/algojudge-toolchain

# Unbuffered, so output that was produced before a timeout is not lost in a
# buffer when the container is killed — the participant needs to see how far
# their program got.
#
# `PYTHONDONTWRITEBYTECODE` stops a run from writing a `__pycache__` beside the
# program it was handed; the build step writes one deliberately, into its own
# output directory, and that is the only one wanted.
ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1

# Where a build puts what it made. Part of the image on purpose: the container
# runs as an unprivileged user and cannot create a directory at the root of its
# own filesystem, so this is the one place the contract has to be declared
# rather than assumed. `aj-standard-io`'s `BUILD_OUTPUT` names the same path.
RUN mkdir -p /out && chown 65534:65534 /out

# Not root, and not able to become root. The sandbox drops every capability
# anyway; this is the second lock on the same door. Debian ships
# `nobody:nogroup` at 65534:65534, so there is no user to add.
# The shim, which a measured run is started through. Root-owned and not
# writable by the user the container runs as: it is ours, not the submission's.
COPY --from=shim --chown=0:0 --chmod=0755 /aj-shim /usr/local/bin/aj-shim

USER 65534:65534
