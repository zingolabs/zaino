#!/usr/bin/env bash
# Build the container image for testing artifacts.
#
# Builds from integration-tests/test_environment, which contains the
# Containerfile and entrypoint.sh for the CI/test environment.
#
# Sourced as the script.main of the `build-image` task (extends
# `base-script`); TAG, IMAGE_NAME, the version vars, info, and
# resolve_build_target come from the base-script pre-script
# (tools/scripts/base-script-pre.sh) and Makefile.toml [env].

set -euo pipefail

# Create target directory with correct ownership before podman creates it as
# root.
mkdir -p target

TARGET=$(resolve_build_target "$ZCASH_VERSION" "$ZEBRA_VERSION")

# For local builds, use the current user's UID/GID to avoid permission
# issues. CI builds will use the default UID=1001 from the Containerfile.

info "Building image"
info "Tag: ${TAG}"
info "Target: $TARGET"
info "Current directory: $(pwd)"
# ls is intentional here: a short human-readable listing for debug output, not
# machine-parsed, so SC2012 (use find) does not apply.
# shellcheck disable=SC2012
info "Files in tools/scripts/: $(ls -la tools/scripts/ | head -5)"

cd integration-tests/test_environment && \
podman build -f Containerfile \
  --target "$TARGET" \
  --build-arg "ZCASH_VERSION=$ZCASH_VERSION" \
  --build-arg "ZEBRA_VERSION=$ZEBRA_VERSION" \
  --build-arg "RUST_VERSION=$RUST_VERSION" \
  --build-arg "UID=$(id -u)" \
  --build-arg "GID=$(id -g)" \
  -t "${IMAGE_NAME}:$TAG" \
  "$@" \
  .
