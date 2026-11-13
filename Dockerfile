# Multi-stage non-root runtime image for `lg` (`specs/0006`, `REQ-LG-CONT-02`).
# Built on Debian trixie with digest-pinned base images, HTTPS apt mirrors,
# `-j 2` Cargo throttling, and UID/GID 65532:65532 (`nonroot`).

# ---- build stage ----
# rust:1.99.1-slim-trixie, resolved 2026-11-13
FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS builder
WORKDIR /build
# OPS-281: in-cluster build environments block port-80 egress, so apt uses HTTPS.
RUN set -eux; \
    if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
      sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources; \
    fi; \
    apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates pkg-config && \
    rm -rf /var/lib/apt/lists/*
# Copy all workspace manifests and local patches. `.dockerignore` excludes
# `rust-toolchain.toml` because the pinned `rust:1.99.1-slim-trixie` image
# already provides `1.99.1`.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY patches ./patches
COPY xtask ./xtask
ENV CARGO_BUILD_JOBS=2
RUN cargo build -j 2 --release --locked -p littleguy --bin lg && \
    strip /build/target/release/lg && \
    install -m 0755 /build/target/release/lg /usr/local/bin/lg

# ---- runtime stage ----
# debian:trixie-slim, resolved 2026-11-13
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS runtime
# Copy the CA bundle first so apt can fetch HTTPS mirrors before `ca-certificates`
# is installed, and run `upgrade -y` so Trivy's HIGH/CRITICAL gate stays green.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN set -eux; \
    if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
      sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources; \
    fi; \
    apt-get -o Acquire::https::CaInfo=/etc/ssl/certs/ca-certificates.crt update && \
    apt-get -o Acquire::https::CaInfo=/etc/ssl/certs/ca-certificates.crt upgrade -y && \
    apt-get -o Acquire::https::CaInfo=/etc/ssl/certs/ca-certificates.crt install -y --no-install-recommends ca-certificates tini && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 65532 nonroot && \
    useradd --uid 65532 --gid 65532 --home-dir /data --no-create-home --shell /usr/sbin/nologin nonroot && \
    install -d -m 0700 -o 65532 -g 65532 /data
COPY --from=builder /usr/local/bin/lg /usr/local/bin/lg
ENV HOME=/data \
    XDG_CONFIG_HOME=/data/config \
    XDG_DATA_HOME=/data \
    USER=nonroot
USER 65532:65532
WORKDIR /data
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/lg"]
CMD ["pool"]
