# syntax=docker/dockerfile:1

############################
# Builder
############################
ARG RUST_VERSION=1.86.0

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
# Runtime
############################
FROM debian:bookworm-slim AS runtime

# Runtime deps
RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
      ca-certificates libssl3 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/bin/zainod /usr/local/bin/zainod

EXPOSE 8137 8237

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

# Run as non-root. The caller MUST map their host user:
#   podman: podman run --userns=keep-id zaino
#   docker: docker run --user "$(id -u):$(id -g)" zaino
CMD ["zainod", "start"]
