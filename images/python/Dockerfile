# The Python runtime a submission is checked and run with.
#
# There is no compilation step, but there **is** a build step: `py_compile`
# turns a syntax error into a `CompilationError` before any test runs. Without
# it every test fails identically with a traceback, and a participant with a
# missing colon is told their answer is wrong on all of them.

FROM python:3-alpine

RUN adduser -D -u 65534 -s /sbin/nologin judge 2>/dev/null || true

RUN python3 --version > /etc/algojudge-toolchain

# Unbuffered, so output that was produced before a timeout is not lost in a
# buffer when the container is killed — the participant needs to see how far
# their program got.
ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1

# Where a build puts what it made. Part of the image on purpose: the container
# runs as an unprivileged user and cannot create a directory at the root of its
# own filesystem, so this is the one place the contract has to be declared
# rather than assumed. `aj-standard-io`'s `BUILD_OUTPUT` names the same path.
RUN mkdir -p /out && chown 65534:65534 /out

USER 65534:65534
