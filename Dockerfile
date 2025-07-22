# syntax=docker/dockerfile:1

# Set the build arguments used across the stages.
# Each stage must define the build arguments (ARGs) it uses.

# Use UID/GID 1000 to match host user
ARG UID=1000
ARG GID=${UID}
ARG USER="container_user"
ARG HOME="/home/container_user"
ARG CARGO_HOME="${HOME}/.cargo"
ARG CARGO_TARGET_DIR="${HOME}/target"

ARG RUST_VERSION=1.86.0

  # This stage prepares Zaino's build deps and captures build args as env vars.
FROM rust:${RUST_VERSION}-bookworm AS deps
SHELL ["/bin/bash", "-xo", "pipefail", "-c"]

# Accept an argument to control no-tls builds
ARG NO_TLS=false
ENV NO_TLS=${NO_TLS:-false}

# Install build deps (if any beyond Rust).
RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-dev \
    gcc \
    clang \
    llvm-dev \
    libclang-dev \
    cmake \
    make \
    # Install OpenSSL only if not building with no-tls
    && if [ "$NO_TLS" = "false" ]; then apt-get install -y libssl-dev; fi \
    && rm -rf /var/lib/apt/lists/*  /tmp/*

# Build arguments and variables
ARG CARGO_INCREMENTAL
ENV CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}

ARG CARGO_HOME
ENV CARGO_HOME=${CARGO_HOME}

ARG CARGO_TARGET_DIR
ENV CARGO_TARGET_DIR=${CARGO_TARGET_DIR}

# This stage builds the zainod release binary.
FROM deps AS builder

# Create container_user for building
ARG UID=1000
ARG GID=1000
ARG USER="container_user"
ARG HOME="/home/container_user"

RUN groupadd --gid ${GID} ${USER} && \
    useradd --uid ${UID} --gid ${GID} --home-dir ${HOME} --create-home ${USER}

WORKDIR ${HOME}/zaino
RUN chown -R ${UID}:${GID} ${HOME}

# Switch to container_user for building
USER ${USER}

ARG CARGO_HOME
ARG CARGO_TARGET_DIR

# Accept an argument to control no-tls builds
ARG NO_TLS=false
ENV NO_TLS=${NO_TLS:-false}

# Mount the root Cargo.toml/Cargo.lock and all relevant workspace members.
RUN --mount=type=bind,source=Cargo.toml,target=Cargo.toml \
    --mount=type=bind,source=Cargo.lock,target=Cargo.lock \
    --mount=type=bind,source=integration-tests,target=integration-tests \
    --mount=type=bind,source=zaino-fetch,target=zaino-fetch \
    --mount=type=bind,source=zaino-proto,target=zaino-proto \
    --mount=type=bind,source=zaino-serve,target=zaino-serve \
    --mount=type=bind,source=zaino-state,target=zaino-state \
    --mount=type=bind,source=zaino-testutils,target=zaino-testutils \
    --mount=type=bind,source=zainod,target=zainod \
    --mount=type=cache,target=${CARGO_HOME} \
    --mount=type=cache,target=${CARGO_TARGET_DIR} \
    # Conditional build based on NO_TLS argument
    if [ "$NO_TLS" = "true" ]; then \
      cargo build --locked --release --package zainod --bin zainod --features disable_tls_unencrypted_traffic_mode; \
    else \
      cargo build --locked --release --package zainod --bin zainod; \
    fi && \
    # Copy the built binary (need root temporarily to write to /usr/local/bin)\
    cp ${CARGO_TARGET_DIR}/release/zainod /tmp/zainod

# This stage prepares the runtime image.
FROM debian:bookworm-slim AS runtime

ARG UID
ARG GID
ARG USER
ARG HOME

RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
    curl \
    openssl \
    libc6 \
    libgcc-s1 && \
    rm -rf /var/lib/apt/lists/* /tmp/*

RUN addgroup --quiet --gid ${GID} ${USER} && \
    adduser --quiet --gid ${GID} --uid ${UID} --home ${HOME} ${USER} --disabled-password --gecos ""

WORKDIR ${HOME}
RUN chown -R ${UID}:${GID} ${HOME}

# Copy the zainod binary from the builder stage
COPY --link --from=builder /tmp/zainod /usr/local/bin/
RUN chmod +x /usr/local/bin/zainod

USER ${USER}

ARG ZAINO_GRPC_PORT=8137
ARG ZAINO_JSON_RPC_PORT=8237

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 CMD curl -f http://127.0.0.1:${ZAINO_GRPC_PORT} || exit 1

# Expose gRPC and JSON-RPC ports if they are typically used.
# These are the default ports zainod might listen on.
EXPOSE ${ZAINO_GRPC_PORT} ${ZAINO_JSON_RPC_PORT}

# Default command if no arguments are passed to `docker run`
CMD ["zainod"]
