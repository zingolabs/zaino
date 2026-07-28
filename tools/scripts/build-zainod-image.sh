#!/usr/bin/env bash
# Build the zainod production container image from the repo-root Dockerfile.
#
# The Dockerfile declares `ARG RUST_VERSION` with no default, deliberately: a
# literal there could drift from rust-toolchain.toml's `channel` unnoticed. The
# cost is that a bare `docker buildx build .` expands the builder stage to
# `rust:-bookworm` and dies with "invalid reference format" — an error that says
# nothing about the missing argument (issue #468).
#
# This script supplies the pin from the same single source of truth CI uses:
# Makefile.toml's [env] RUST_VERSION, derived from the workbench
# get-rust-version binary.
#
# Run via `makers build-zainod-image`. Extra arguments are forwarded to
# `docker buildx build`, so a no-TLS image is:
#
#   makers build-zainod-image -- --build-arg NO_TLS=true

set -euo pipefail

if [ -z "${RUST_VERSION:-}" ]; then
  echo "RUST_VERSION is unset; run this through 'makers build-zainod-image' so" >&2
  echo "Makefile.toml's [env] can derive it from rust-toolchain.toml." >&2
  exit 1
fi

IMAGE="${ZAINOD_IMAGE:-zaino:latest}"

echo "Building ${IMAGE} (RUST_VERSION=${RUST_VERSION})"

docker buildx build \
  --build-arg "RUST_VERSION=${RUST_VERSION}" \
  --tag "${IMAGE}" \
  "$@" \
  .

echo "Built ${IMAGE}"
