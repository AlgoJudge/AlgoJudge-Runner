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
USER 65534:65534
