# syntax=docker/dockerfile:1.7
#
# The published image runs the same gateway the release binaries do: one static
# binary with the dashboard embedded. Three stages keep the Node and Rust
# toolchains out of the runtime layer, which is the whole point of the split.

ARG RUST_IMAGE=rust:1.88-bookworm
ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG APP_VERSION=unknown

# --- dashboard -------------------------------------------------------------
# `router-server` embeds `web/dist` with rust-embed at compile time, so the
# Vite output has to exist before the binary is built.
FROM ${NODE_IMAGE} AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# --- binary ----------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
# aws-lc-sys (the rustls backend) compiles C and assembly through cmake and
# needs clang as its assembler; libsqlite3-sys builds the bundled SQLite, so no
# system libsqlite3 is linked in or needed at runtime.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=web /web/dist ./web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rustrouter \
 && cp target/release/rustrouter /usr/local/bin/rustrouter

# --- runtime ---------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
ARG APP_VERSION
LABEL org.opencontainers.image.title="rustrouter" \
      org.opencontainers.image.description="Local AI routing gateway plus dashboard" \
      org.opencontainers.image.version="${APP_VERSION}" \
      org.opencontainers.image.licenses="MIT"

# `resolve_host()` reads HOSTNAME, and Docker sets that to the container id, so
# it has to be pinned or the gateway would try to bind an address that does not
# exist. PORT and DATA_DIR mirror the 9router image; the port differs because
# rustrouter runs on 20129.
ENV PORT=20129 \
    HOSTNAME=0.0.0.0 \
    DATA_DIR=/app/data \
    RUSTROUTER_INSTALL_METHOD=docker

COPY --from=build /usr/local/bin/rustrouter /usr/local/bin/rustrouter

# `ca-certificates` is not optional. The TLS verifier is
# rustls-platform-verifier -> rustls-native-certs -> openssl-probe, which reads
# the OS trust store; debian-slim is a minbase image with no such store, and
# reqwest builds the verifier eagerly, so without it every outbound HTTPS call
# fails at client construction. The localhost health check would not notice.
# `hostname` backs the last fallback in the machine-id chain, which is the
# derivation the CLI token depends on.
#
# The entrypoint drops privileges after taking ownership of the data dir, which
# is how a bind mount owned by the host user still works. `gosu` is the Debian
# counterpart of the Alpine `su-exec` the 9router image uses. Started with
# `--user`, the chown is skipped and the process runs as that uid.
#
# HOME is a writable directory of its own because the CLI-tool writers under
# `$HOME` are the one feature that does not route through DATA_DIR; with the
# default `/root` they would fail for uid 10001.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gosu ca-certificates hostname \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin rustrouter \
 && mkdir -p /app/data /app/data-home \
 && chown -R rustrouter:rustrouter /app \
 && printf '#!/bin/sh\nset -e\nif [ "$(id -u)" = "0" ]; then\n  chown -R rustrouter:rustrouter /app/data /app/data-home 2>/dev/null || true\n  exec gosu rustrouter "$@"\nfi\nexec "$@"\n' > /entrypoint.sh \
 && chmod +x /entrypoint.sh

ENV HOME=/app/data-home

WORKDIR /app
VOLUME ["/app/data"]
EXPOSE 20129

ENTRYPOINT ["/entrypoint.sh"]
CMD ["rustrouter", "serve"]
