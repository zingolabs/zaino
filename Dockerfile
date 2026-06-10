# syntax=docker/dockerfile:1

############################
# Global build args
############################
# RUST_VERSION must be supplied via --build-arg. Canonical source is
# rust-toolchain.toml's `channel`, surfaced by tools/scripts/get-rust-version.sh
# — no default is set so a stale literal cannot drift from the workspace's
# pinned toolchain. See README for the recommended build invocation.
ARG RUST_VERSION
ARG UID=1000
ARG GID=1000
ARG USER=container_user
ARG HOME=/home/container_user

############################
# Builder
############################
FROM docker.io/library/rust:${RUST_VERSION}-bookworm AS builder
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
WORKDIR /app

# Toggle to build without TLS feature if needed
ARG NO_TLS=false

# Build deps incl. protoc for prost-build
# Versions pinned (DL3008) for reproducibility / supply-chain hygiene. Pins
# match the candidate versions in docker.io/library/rust:1.95.0-bookworm; bump
# them together with the base image (query with `apt-cache policy <pkg>`).
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config=1.8.1-1 \
      clang=1:14.0-55.7~deb12u1 \
      cmake=3.25.1-1 \
      make=4.3-4.1 \
      libssl-dev=3.0.20-1~deb12u1 \
      ca-certificates=20230311+deb12u1 \
      protobuf-compiler=3.21.12-3 \
  && rm -rf /var/lib/apt/lists/*

# Copy entire workspace (prevents missing members)
COPY . .

# Efficient caches + install to a known prefix (/out)
# This avoids relying on target/release/<bin> paths.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ "${NO_TLS}" = "true" ]; then \
      cargo install --locked --path packages/zainod --bin zainod --root /out --features no_tls_use_unencrypted_traffic; \
    else \
      cargo install --locked --path packages/zainod --bin zainod --root /out; \
    fi

############################
# Runtime
############################
FROM docker.io/library/debian:bookworm-slim AS runtime
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

ARG UID
ARG GID
ARG USER
ARG HOME

# Runtime deps
# Versions pinned (DL3008) to the candidates in
# docker.io/library/debian:bookworm-slim; bump together with the base image.
RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
      ca-certificates=20230311+deb12u1 \
      libssl3=3.0.20-1~deb12u1 \
      libgcc-s1=12.2.0-14+deb12u1 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN addgroup --gid "${GID}" "${USER}" && \
    adduser  --uid "${UID}" --gid "${GID}" --home "${HOME}" \
             --disabled-password --gecos "" "${USER}"

ENV HOME=${HOME}

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

USER ${USER}

ENTRYPOINT ["/entrypoint.sh"]
CMD ["start"]
