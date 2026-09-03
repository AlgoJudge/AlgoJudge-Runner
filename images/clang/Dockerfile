# The LLVM toolchain a C or C++ submission is built and run with.
#
# The catalogue offers every standard through both compilers, because `g++` and
# `clang++` disagree about enough that a submission accepted by one is
# occasionally rejected by the other — which is a thing a course teaching a
# standard may legitimately want to pin.
#
# See `images/gcc/Dockerfile` for why the base is Debian trixie and what
# `-static` costs. The short version: Clang plus static linking is less certain
# on musl than on glibc, and `-std=c23` needs a Clang newer than bookworm's.

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

FROM debian:trixie-slim

# Clang on Debian compiles C++ against GNU's libstdc++ rather than shipping its
# own, and `-static` links the static half of it. **`clang` depends on
# `libstdc++-14-dev`**, so naming it here changes nothing today — verified by
# building without it, which still produced a working image. It is named as a
# pin: if that dependency is ever dropped, this fails at `docker build` instead
# of turning every C++ row into a link error somebody reads as their own.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libc6-dev libstdc++-14-dev \
 && rm -rf /var/lib/apt/lists/*

# **Nothing reads this file.** It said it was reported to the Server as a
# capability and written into the result's log, and neither happens: the
# registration payload has no field for a toolchain, and the only attachments a
# result carries are the compiler's own diagnostics and the details document.
# Kept because `docker run --rm <image> cat /etc/algojudge-toolchain` is how a
# person checks what an image actually shipped. Corrected 2026-08-31.
RUN clang --version | head -1 > /etc/algojudge-toolchain

# Where a build puts what it made. See `images/gcc/Dockerfile`.
RUN mkdir -p /out && chown 65534:65534 /out

# The shim, which a measured run is started through. Root-owned and not
# writable by the user the container runs as: it is ours, not the submission's.
COPY --from=shim --chown=0:0 --chmod=0755 /aj-shim /usr/local/bin/aj-shim

USER 65534:65534
