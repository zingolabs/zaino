# syntax=docker/dockerfile:1

############################
# Global build args
############################
ARG RUST_VERSION=1.86.0
ARG UID=1000
ARG GID=1000
ARG USER=container_user
ARG HOME=/home/container_user

############################
# Planner — extract dependency recipe from full source
############################
FROM rust:${RUST_VERSION}-bookworm AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

############################
# Cook — build dependencies only (cached when Cargo.toml/lock unchanged)
############################
FROM rust:${RUST_VERSION}-bookworm AS cook
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
WORKDIR /app

ARG NO_TLS=false

# Build deps incl. protoc for prost-build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config clang cmake make libssl-dev ca-certificates \
      protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked

# Copy only the dependency recipe — this layer is invalidated only when
# Cargo.toml, Cargo.lock, or workspace member manifests change.
COPY --from=planner /app/recipe.json recipe.json

# Cook dependencies. The --mount caches still help when using BuildKit;
# kaniko ignores them but the layer cache does the heavy lifting instead.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ "${NO_TLS}" = "true" ]; then \
      cargo chef cook --release --recipe-path recipe.json \
        --package zainod --features no_tls_use_unencrypted_traffic; \
    else \
      cargo chef cook --release --recipe-path recipe.json \
        --package zainod; \
    fi

############################
# Builder — compile source (dependencies already built)
############################
FROM cook AS builder
WORKDIR /app

ARG NO_TLS=false

# Copy full source on top of cooked dependencies
COPY . .

# Build the binary. Dependencies are already compiled — only zainod source is rebuilt.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ "${NO_TLS}" = "true" ]; then \
      cargo install --locked --path zainod --bin zainod --root /out --features no_tls_use_unencrypted_traffic; \
    else \
      cargo install --locked --path zainod --bin zainod --root /out; \
    fi

############################
# Runtime
############################
FROM debian:bookworm-slim AS runtime
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

ARG UID
ARG GID
ARG USER
ARG HOME

# Runtime deps + setpriv for privilege dropping
RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
      ca-certificates libssl3 libgcc-s1 util-linux \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user (entrypoint will drop privileges to this user)
RUN addgroup --gid "${GID}" "${USER}" && \
    adduser  --uid "${UID}" --gid "${GID}" --home "${HOME}" \
             --disabled-password --gecos "" "${USER}"

# Make UID/GID available to entrypoint
ENV UID=${UID} GID=${GID} HOME=${HOME}

WORKDIR ${HOME}

# Create ergonomic mount points with symlinks to XDG defaults
# Users mount to /app/config and /app/data, zaino uses ~/.config/zaino and ~/.cache/zaino
RUN mkdir -p /app/config /app/data && \
    mkdir -p ${HOME}/.config ${HOME}/.cache && \
    ln -s /app/config ${HOME}/.config/zaino && \
    ln -s /app/data ${HOME}/.cache/zaino && \
    chown -R ${UID}:${GID} /app ${HOME}/.config ${HOME}/.cache

# Copy binary and entrypoint
COPY --from=builder /out/bin/zainod /usr/local/bin/zainod
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Default ports
ARG ZAINO_GRPC_PORT=8137
ARG ZAINO_JSON_RPC_PORT=8237
EXPOSE ${ZAINO_GRPC_PORT} ${ZAINO_JSON_RPC_PORT}

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

# Start as root; entrypoint drops privileges after setting up directories
ENTRYPOINT ["/entrypoint.sh"]
CMD ["start"]
