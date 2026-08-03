#!/usr/bin/env bash
# Build the zaino-ci container image (the CI Rust build environment) from
# live-tests/test_environment/Containerfile.
#
# Sourced as the script.main of the `build-image` task (extends `base-script`);
# TAG, IMAGE_NAME, RUST_VERSION, and info come from the base-script pre-script
# (tools/scripts/base-script-pre.sh) and Makefile.toml [env].

set -euo pipefail

info "Building image"
info "Tag: ${TAG}"
info "Current directory: $(pwd)"

# For local builds, use the current user's UID/GID to avoid permission issues;
# CI builds fall back to the Containerfile default UID/GID.
cd live-tests/test_environment && \
podman build -f Containerfile \
  --build-arg "RUST_VERSION=$RUST_VERSION" \
  --build-arg "UID=$(id -u)" \
  --build-arg "GID=$(id -g)" \
  -t "${IMAGE_NAME}:$TAG" \
  "$@" \
  .
