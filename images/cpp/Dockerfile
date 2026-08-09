# The C++ toolchain a submission is built and run with.
#
# Ours rather than a stock compiler image, for two reasons the analysis names:
# language images became this project's responsibility when Judge0 left the MVP
# (D-3), and the compiler version is **reported as a capability**, so it has to
# be a thing we pin rather than a thing that moves under us.
#
# Alpine because the run container is started once per test and image size is
# start-up latency. `g++` from Alpine links against musl, which is also why the
# compile step below is static: the run container mounts the binary and nothing
# else, so it cannot rely on the image's libraries being where the compiler left
# them.

FROM alpine:3

RUN apk add --no-cache g++ musl-dev

# Not root, and not able to become root. The sandbox drops every capability
# anyway; this is the second lock on the same door.
RUN adduser -D -u 65534 -s /sbin/nologin judge 2>/dev/null || true

# Reported to the Server as this Runner's capability, and written into the
# result's log so a verdict can be traced to the compiler that produced it.
RUN g++ --version | head -1 > /etc/algojudge-toolchain

# Where a build puts what it made. Part of the image on purpose: the container
# runs as an unprivileged user and cannot create a directory at the root of its
# own filesystem, so this is the one place the contract has to be declared
# rather than assumed. `aj-standard-io`'s `BUILD_OUTPUT` names the same path.
RUN mkdir -p /out && chown 65534:65534 /out

USER 65534:65534
