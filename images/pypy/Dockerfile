# The PyPy runtime a submission is checked and run with.
#
# The same language as `images/python` and a different implementation of it.
# Offered because the gap is large enough to change what a participant can do:
# a Python solution that times out on CPython frequently does not on PyPy, and a
# course that wants Python to be a real choice for a tight limit needs the
# second row rather than a longer `overrideLimits`.
#
# **PyPy is the reason the language images are Debian at all.** There is no
# official musl build of it, so on Alpine this image could not exist — see
# `images/gcc/Dockerfile` for the whole of that decision.

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

FROM pypy:3-slim

RUN pypy3 --version | head -1 > /etc/algojudge-toolchain

# Unbuffered, so output that was produced before a timeout is not lost in a
# buffer when the container is killed. PyPy buffers more aggressively than
# CPython, so this matters more here rather than less.
ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1

# Where a build puts what it made. See `images/gcc/Dockerfile`.
RUN mkdir -p /out && chown 65534:65534 /out

# The shim, which a measured run is started through. Root-owned and not
# writable by the user the container runs as: it is ours, not the submission's.
COPY --from=shim --chown=0:0 --chmod=0755 /aj-shim /usr/local/bin/aj-shim

USER 65534:65534
