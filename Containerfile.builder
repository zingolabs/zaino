# syntax=docker/dockerfile:1

############################
# Builder
############################
ARG RUST_VERSION

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
