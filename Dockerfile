# syntax=docker/dockerfile:1@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
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

FROM rust:1.96-slim@sha256:31ee7fc65186be7e0e0ccb3f2ca305f14e4739e7642a1ae65753aa5d7b874523 AS builder
WORKDIR /build

# Bring the whole self-contained crate (the binary + the four vendored surface crates under
# crates/) and build the release binary. The surfaces' static/ + templates/ are needed at build
# time for their include_str! embeds.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
RUN cargo build --release --locked --bin concourse \
    && strip target/release/concourse

FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd AS runtime
ARG VCS_REF=unknown
LABEL org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.source="https://git.w33d.xyz/git/w33d/concourse.git"
# Bootstrap HTTPS from the CA bundle carried by the pinned builder image. The runtime then replaces
# it with the exact package from the fixed Debian snapshot below.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN <<'EOF'
set -eux
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources
cat > /etc/apt/sources.list.d/debian-snapshot.sources <<'SOURCES'
Types: deb
URIs: https://snapshot.debian.org/archive/debian/20260720T000000Z
Suites: trixie
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
Check-Valid-Until: no
SOURCES
cat > /etc/apt/apt.conf.d/99bootstrap-ca <<'APT'
Acquire::https::CaInfo "/etc/ssl/certs/ca-certificates.crt";
Acquire::https::Verify-Peer "true";
Acquire::https::Verify-Host "true";
APT
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates=20250419 \
    libssl3t64=3.5.6-1~deb13u2 \
    openssl=3.5.6-1~deb13u2 \
    openssl-provider-legacy=3.5.6-1~deb13u2
rm -f /etc/apt/apt.conf.d/99bootstrap-ca
rm -rf /var/lib/apt/lists/*
EOF

# Non-root runtime user (no shell, no home writes needed).
RUN groupadd --system --gid 10001 concourse \
    && useradd --system --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin concourse
COPY --from=builder /build/target/release/concourse /usr/local/bin/concourse

USER concourse
ENV BIND_ADDR=0.0.0.0:9050
EXPOSE 9050

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["concourse", "healthcheck"]

CMD ["concourse"]
