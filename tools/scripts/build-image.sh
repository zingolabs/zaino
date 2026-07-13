#!/usr/bin/env bash
# Build the container image for testing artifacts.
#
# Builds from live-tests/test_environment, which contains the
# Containerfile and entrypoint.sh for the CI/test environment.
#
# Sourced as the script.main of the `build-image` task (extends
# `base-script`); TAG, IMAGE_NAME, the version vars, info, and
# resolve_build_target come from the base-script pre-script
# (tools/scripts/base-script-pre.sh) and Makefile.toml [env].

set -euo pipefail

# Shared with get-ci-image-tag.sh so the tag's devtool SHA and this build's
# checkout SHA are resolved identically.
# shellcheck source=tools/scripts/functions.sh
source ./tools/scripts/functions.sh

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

# Resolve DEVTOOL_VERSION (a moving branch) to its current commit SHA so the
# devtool build layer's cache key tracks the branch HEAD. Same resolution the
# tag uses, so the build-arg SHA and the tag's embedded SHA match.
DEVTOOL_REV=$(resolve_devtool_rev "$DEVTOOL_VERSION")
info "Resolved DEVTOOL_VERSION=$DEVTOOL_VERSION to DEVTOOL_REV=$DEVTOOL_REV"

# Resolve ZEBRA_VERSION (canonically the Docker Hub tag, e.g. "6.0.0-rc.0")
# to the git ref the zebra-builder source stage checks out — zebra's release
# tags are `v`-prefixed in git. Fails here, before the build, on an
# unresolvable value.
ZEBRA_GIT_REF=$(cargo run -q --manifest-path tools/workbench/Cargo.toml \
  --bin get-zebra-git-ref -- "$ZEBRA_VERSION")
info "Resolved ZEBRA_VERSION=$ZEBRA_VERSION to ZEBRA_GIT_REF=$ZEBRA_GIT_REF"

cd live-tests/test_environment && \
podman build -f Containerfile \
  --target "$TARGET" \
  --build-arg "ZCASH_VERSION=$ZCASH_VERSION" \
  --build-arg "ZEBRA_VERSION=$ZEBRA_VERSION" \
  --build-arg "ZEBRA_GIT_REF=$ZEBRA_GIT_REF" \
  --build-arg "DEVTOOL_VERSION=$DEVTOOL_VERSION" \
  --build-arg "DEVTOOL_REV=$DEVTOOL_REV" \
  --build-arg "RUST_VERSION=$RUST_VERSION" \
  --build-arg "UID=$(id -u)" \
  --build-arg "GID=$(id -g)" \
  -t "${IMAGE_NAME}:$TAG" \
  "$@" \
  .
