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
# Builder
############################
FROM rust:${RUST_VERSION}-bookworm AS builder
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
WORKDIR /app

# Toggle to build without TLS feature if needed
ARG NO_TLS=false

# Build deps incl. protoc for prost-build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config clang cmake make libssl-dev ca-certificates \
      protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

# Copy entire workspace (prevents missing members)
COPY . .

# Efficient caches + install to a known prefix (/out)
# This avoids relying on target/release/<bin> paths.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ "${NO_TLS}" = "true" ]; then \
      cargo install --locked --path zainod --bin zainod --root /out --features no_tls_use_unencrypted_traffic; \
    else \
      cargo install --locked --path zainod --bin zainod --root /out; \
    fi

############################
# Runtime (slim, non-root)
############################
FROM debian:bookworm-slim AS runtime
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

ARG UID
ARG GID
ARG USER
ARG HOME

# Only the dynamic libs needed by a Rust/OpenSSL binary
RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
      ca-certificates libssl3 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN addgroup --gid "${GID}" "${USER}" && \
    adduser  --uid "${UID}" --gid "${GID}" --home "${HOME}" \
             --disabled-password --gecos "" "${USER}"

WORKDIR ${HOME}

# Copy the installed binary from builder
COPY --from=builder /out/bin/zainod /usr/local/bin/zainod

RUN chown -R "${UID}:${GID}" "${HOME}"
USER ${USER}

# Default ports (adjust if your app uses different ones)
ARG ZAINO_GRPC_PORT=8137
ARG ZAINO_JSON_RPC_PORT=8237
EXPOSE ${ZAINO_GRPC_PORT} ${ZAINO_JSON_RPC_PORT}

# Healthcheck that doesn't assume specific HTTP/gRPC endpoints
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

CMD ["zainod"]
