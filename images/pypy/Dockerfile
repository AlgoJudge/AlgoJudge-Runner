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

FROM pypy:3-slim

RUN pypy3 --version | head -1 > /etc/algojudge-toolchain

# Unbuffered, so output that was produced before a timeout is not lost in a
# buffer when the container is killed. PyPy buffers more aggressively than
# CPython, so this matters more here rather than less.
ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1

# Where a build puts what it made. See `images/gcc/Dockerfile`.
RUN mkdir -p /out && chown 65534:65534 /out

USER 65534:65534
