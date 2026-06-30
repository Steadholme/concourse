# syntax=docker/dockerfile:1
#
# Multi-stage build for Concourse (chat/cal/notify/inbox vhost demux over four vendored library
# crates).
#   - builder: rust:1.96-slim (Debian trixie).
#   - runtime: debian:trixie-slim (matching glibc), non-root, ca-certificates.
#
# The surfaces embed their templates + static assets via include_str! at COMPILE time, so the
# runtime image carries only the single statically-templated binary — no assets to ship. sqlx uses
# rustls (no OpenSSL); Klaxon's webhook/SMTP egress and every audit emitter are hand-rolled HTTP/1.1
# over raw TCP sockets, so the binary depends only on glibc — no libssl. ca-certificates is kept
# because the audit emitters post to Watchtower. The HEALTHCHECK uses the built-in
# `concourse healthcheck` subcommand, so the image needs no curl.

FROM rust:1.96-slim AS builder
WORKDIR /build

# Bring the whole self-contained crate (the binary + the four vendored surface crates under
# crates/) and build the release binary. The surfaces' static/ + templates/ are needed at build
# time for their include_str! embeds.
COPY Cargo.toml ./
COPY src ./src
COPY crates ./crates
RUN cargo build --release --bin concourse \
    && strip target/release/concourse

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (no shell, no home writes needed).
RUN useradd --system --uid 10001 --user-group --no-create-home concourse
COPY --from=builder /build/target/release/concourse /usr/local/bin/concourse

USER concourse
ENV BIND_ADDR=0.0.0.0:9050
EXPOSE 9050

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["concourse", "healthcheck"]

CMD ["concourse"]
