# The Runner, as one static binary in an image with nothing else in it.
#
# `musl` rather than glibc so the binary carries no dynamic loader, which is
# what lets the final stage be a distroless image with no shell, no package
# manager and nothing to pivot from. The Runner holds the container runtime's
# socket, so what is in its image matters more than it would elsewhere.

FROM rust@sha256:3b2879047d42784ca9403ad20c51ed3df361a50f1df96f5777d39b4e33aa65cd AS build

# `musl-gcc` is needed by the one C dependency in the graph — `ring`, under
# rustls. Everything else is pure Rust.
RUN apt-get update \
    && apt-get install --no-install-recommends -y musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /src

# Manifests first, so a change to the source does not re-resolve or re-download
# the dependency graph.
COPY Cargo.toml Cargo.lock ./
COPY crates/aj-output-only/Cargo.toml crates/aj-output-only/
COPY crates/aj-package/Cargo.toml crates/aj-package/
COPY crates/aj-protocol/Cargo.toml crates/aj-protocol/
COPY crates/aj-runner/Cargo.toml crates/aj-runner/
COPY crates/aj-sandbox/Cargo.toml crates/aj-sandbox/
COPY crates/aj-standard-io/Cargo.toml crates/aj-standard-io/
# Discovered rather than listed. A named list went stale the first time a crate
# was added, and the image quietly kept building the previous binary — the tests
# then failed somewhere else entirely. A missing `COPY` above now fails here
# instead, because cargo cannot load a workspace member whose manifest is absent.
RUN for dir in crates/*/; do \
        mkdir -p "$dir/src" && echo '' > "$dir/src/lib.rs"; \
    done \
    && mkdir -p crates/aj-runner/src \
    && echo 'fn main() {}' > crates/aj-runner/src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && rm -r crates/*/src

COPY crates crates
# The stubs' fingerprints would otherwise let cargo think the crates are
# current. `find` rather than a list, so adding a crate does not silently ship a
# binary built from an empty one.
RUN find crates -name '*.rs' -exec touch {} + \
    && cargo build --release --target x86_64-unknown-linux-musl

# The two directories the Runner writes to, created here so that a **named
# volume mounted over either one inherits this ownership**. Docker seeds an
# empty volume from the image path, and a directory that does not exist in the
# image gives a volume owned by root — which a `nonroot` process cannot write,
# and which fails at the first thing the Runner does.
RUN mkdir -p /state/lib /state/cache

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=build \
    /src/target/x86_64-unknown-linux-musl/release/algojudge-runner \
    /usr/local/bin/algojudge-runner

COPY --from=build --chown=65532:65532 /state/lib /var/lib/algojudge-runner
COPY --from=build --chown=65532:65532 /state/cache /var/cache/algojudge-runner

# The identity and the cache are state; both are meant to be volumes. Losing the
# cache costs a download, losing the key costs a re-registration and an
# administrator's approval.
ENV AJ_Runner__KeyPath=/var/lib/algojudge-runner/identity.key \
    AJ_Cache__Path=/var/cache/algojudge-runner

# No port is published, and none is listened on: the Runner dials out, which is
# the whole reason one can sit behind a domestic router.

USER nonroot
ENTRYPOINT ["/usr/local/bin/algojudge-runner"]
