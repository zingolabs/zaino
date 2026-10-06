############################
# Global build args
############################
ARG UID=1000
ARG GID=1000
ARG USER=container_user
ARG HOME=/home/container_user
# zainod cargo features, comma-separated (`snapshot` also installs aria2c at runtime)
ARG CARGO_FEATURES=""

############################
# Builder
############################
FROM docker.io/library/rust:1.98.0-bookworm AS builder
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
WORKDIR /app

# `release` or `profiling` (adds line tables + frame pointers, set below)
ARG CARGO_PROFILE=release
ARG CARGO_FEATURES

# Build deps incl. protoc for prost-build
# Versions pinned (DL3008) for reproducibility / supply-chain hygiene. Pins
# match the candidate versions in docker.io/library/rust:1.98.0-bookworm; bump
# them together with the base image (query with `apt-cache policy <pkg>`).
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config=1.8.1-1 \
      make=4.3-4.1 \
      ca-certificates=20250419~deb12u1 \
      protobuf-compiler=3.21.12-3+deb12u1 \
  && rm -rf /var/lib/apt/lists/*

# rust-toolchain.toml lists these components; installing them here keeps
# the download in a cached layer instead of re-syncing the channel on
# every build of the workspace layer below.
RUN rustup component add clippy rustfmt

# Copy entire workspace (prevents missing members)
COPY . .

# Efficient caches + install to a known prefix (/out)
# This avoids relying on target/release/<bin> paths.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ "${CARGO_PROFILE}" = "profiling" ]; then \
      export RUSTFLAGS="-C force-frame-pointers=yes"; \
    fi; \
    cargo install --locked --path packages/zainod --bin zainod --root /out \
      --profile "${CARGO_PROFILE}" ${CARGO_FEATURES:+--features "${CARGO_FEATURES}"}

############################
# Runtime
############################
FROM docker.io/library/debian:bookworm-slim AS runtime
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

ARG UID
ARG GID
ARG USER
ARG HOME
ARG CARGO_FEATURES

# Runtime deps
# Versions pinned (DL3008) to the candidates in
# docker.io/library/debian:bookworm-slim; bump together with the base image.
RUN apt-get -qq update && \
    apt-get -qq install -y --no-install-recommends \
      ca-certificates=20250419~deb12u1 \
      libgcc-s1=12.2.0-14+deb12u1 \
    && rm -rf /var/lib/apt/lists/*

# aria2c: the `snapshot` feature's downloader (`[snapshot]` bootstrap)
RUN if [[ ",${CARGO_FEATURES}," == *",snapshot,"* ]]; then \
      apt-get -qq update && \
      apt-get -qq install -y --no-install-recommends aria2=1.36.0-1 && \
      rm -rf /var/lib/apt/lists/*; \
    fi

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

COPY --from=builder /out/bin/zainod /usr/local/bin/zainod

# Default port
ARG ZAINO_GRPC_PORT=8137
EXPOSE ${ZAINO_GRPC_PORT}

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

USER ${USER}

# Config at /app/config/zainod.toml (zainod's default path), data under /app/data
ENTRYPOINT ["zainod"]
CMD ["start"]
