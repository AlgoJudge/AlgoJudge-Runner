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

# Reported to the Server as this Runner's capability, and written into the
# result's log so a verdict can be traced to the compiler that produced it.
RUN clang --version | head -1 > /etc/algojudge-toolchain

# Where a build puts what it made. See `images/gcc/Dockerfile`.
RUN mkdir -p /out && chown 65534:65534 /out

USER 65534:65534
