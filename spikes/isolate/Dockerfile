# isolate, built from a pinned tag, for the spike D-6 asks for.
#
# Not a production image and not referenced by the Runner. It exists so the
# probe beside it can be re-run by somebody who does not believe the numbers in
# `docs/spikes/ISOLATE.md` — which is the only reason a spike is worth keeping
# after it has answered its question.
FROM debian:bookworm-slim

# `ca-certificates` is not optional here even though apt itself works without
# it: the clone below is HTTPS, and its absence fails as a git exit code 128
# with no message about certificates at all.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential git ca-certificates pkg-config \
      libcap-dev libseccomp-dev libsystemd-dev \
    && rm -rf /var/lib/apt/lists/*

# Pinned. `main` moves, and a spike whose subject changes under it answers
# nothing. v2.6 is the current tag as of 2026-08-09.
ARG ISOLATE_REF=v2.6
RUN git clone --quiet --depth 1 --branch ${ISOLATE_REF} https://github.com/ioi/isolate /src

WORKDIR /src
RUN make isolate isolate-cg-keeper

# setuid root, which is how upstream ships it: isolate drops to an unprivileged
# uid for the child itself rather than being started unprivileged.
RUN install -m 4755 -o root -g root isolate /usr/local/bin/isolate && \
    install -m 0755 isolate-check-environment /usr/local/bin/ && \
    mkdir -p /usr/local/etc && \
    sed -e 's|@BOXDIR@|/var/local/lib/isolate|' default.cf.in > /usr/local/etc/isolate

# The shipped configuration reads its uid range from /etc/subuid via an
# `isolate` user that this image has no reason to create. An explicit range
# says the same thing without inventing an account.
RUN sed -i \
      -e 's/^subid_user = isolate/# subid_user: replaced by an explicit range below/' \
      -e 's/^# first_uid = 60000/first_uid = 60000/' \
      -e 's/^# first_gid = 60000/first_gid = 60000/' \
      -e 's/^# num_boxes = 1000/num_boxes = 16/' \
      /usr/local/etc/isolate

RUN mkdir -p /var/local/lib/isolate /run/isolate/locks

COPY probe.sh /usr/local/bin/probe.sh
RUN chmod +x /usr/local/bin/probe.sh

# The default command, so nobody has to name the path. On a Git Bash host an
# absolute path in `docker run` is rewritten into a Windows one and the
# container fails with a puzzling "no such file or directory" naming a
# `C:/Program Files/Git/...` path that was never asked for.
CMD ["/usr/local/bin/probe.sh"]
