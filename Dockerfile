# Multi-stage build: pinned toolchain builder -> slim runtime.
#
# The builder tag must match rust-toolchain.toml; the toolchain file is
# copied in so a mismatched base image fails fast instead of compiling
# with a different rustc than CI verified.

# UI build stage: the SPA is embedded into the server binary at compile
# time, so it must be built first.
FROM node:22-bookworm-slim AS ui-builder
WORKDIR /build/ui
COPY ui/package.json ui/package-lock.json ./
RUN --mount=type=secret,id=extra_ca_certs \
    if [ -f /run/secrets/extra_ca_certs ]; then \
      npm config set cafile /run/secrets/extra_ca_certs; \
    fi \
    && npm ci --no-audit --no-fund
COPY ui/ ./
RUN npm run build

FROM rust:1.97.1-bookworm AS builder
WORKDIR /build

# Building behind a TLS-inspecting proxy: pass the proxy's CA bundle with
#   docker build --secret id=extra_ca_certs,src=proxy-ca.pem .
# (omitted entirely on a normal network — the mount is optional)
RUN --mount=type=secret,id=extra_ca_certs \
    if [ -f /run/secrets/extra_ca_certs ]; then \
      cp /run/secrets/extra_ca_certs \
        /usr/local/share/ca-certificates/extra-proxy-ca.crt \
      && update-ca-certificates; \
    fi

# The image already ships this exact toolchain (minimal profile); pinning it
# here stops rustup from re-syncing the channel to fetch the rustfmt/clippy
# components rust-toolchain.toml requests — the build needs neither.
ENV RUSTUP_TOOLCHAIN=1.97.1

COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates
COPY ee ./ee
COPY config ./config
COPY docs ./docs
COPY --from=ui-builder /build/ui/dist ./ui/dist

RUN cargo build --release --locked -p merge0-server -p merge0-hosted

# Runtime: no toolchain, no source — just the binaries, the versioned
# config, and CA roots for outbound TLS (GitHub / vendor APIs / Slack).
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /app --shell /usr/sbin/nologin merge0
WORKDIR /app

COPY --from=builder /build/target/release/merge0-server /usr/local/bin/merge0-server
COPY --from=builder /build/target/release/merge0-hosted /usr/local/bin/merge0-hosted
COPY --from=builder /build/config /app/config

USER merge0
ENV MERGE0_CONFIG_DIR=/app/config
# Inside a container the loopback default is unreachable; bind the pod
# interface and let the orchestrator (compose / Coolify) firewall it.
ENV MERGE0_BIND=0.0.0.0:8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/merge0-server"]
