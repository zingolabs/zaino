#!/usr/bin/env bash

# ------- HELPERS ------------

info() {
  echo -e "\033[1;36m\033[1m>>> $1\033[0m"
}

warn() {
  echo -e "\033[1;33m\033[1m>>> $1\033[0m"
}

err() {
  echo -e "\033[1;31m\033[1m>>> $1\033[0m"
}

is_tag() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

resolve_build_target() {
  local zcash="$1"
  local zebra="$2"

  if is_tag "$zcash" && is_tag "$zebra"; then
    echo "final-prebuilt"
  elif ! is_tag "$zcash" && is_tag "$zebra"; then
    echo "final-zcashd-source"
  elif is_tag "$zcash" && ! is_tag "$zebra"; then
    echo "final-zebrad-source"
  else
    echo "final-all-source"
  fi
}

# ------- ZAINOD CONTAINER HELPERS ------------

# Validate the container engine argument.
# Sets ENGINE as a side-effect.
validate_engine() {
  ENGINE="${1:?Usage: makers <task> <podman|docker>}"
  if [ "$ENGINE" != "podman" ] && [ "$ENGINE" != "docker" ]; then
    err "Unknown engine: $ENGINE (use podman or docker)"
    exit 1
  fi
}

# Abort unless the working tree is clean.
# Respects FORCE=true to override.
require_clean_worktree() {
  if [ -n "$(git status --porcelain)" ]; then
    if [ "${FORCE:-}" = "true" ]; then
      warn "Working directory is dirty — proceeding because FORCE=true"
    else
      err "Working directory is dirty. Commit your changes or set FORCE=true"
      exit 1
    fi
  fi
}

# Compute image tags from the current rust toolchain and HEAD commit.
# Exports: RUST_VERSION, COMMIT, BUILDER_TAG, RUNTIME_TAG, ZAINOD_TAG
compute_zainod_tags() {
  RUST_VERSION=$(rustc --version | awk '{print $2}')
  COMMIT=$(git rev-parse --short HEAD)
  BUILDER_TAG="zaino-builder:${RUST_VERSION}-${COMMIT}"
  RUNTIME_TAG="zaino-runtime:${RUST_VERSION}-${COMMIT}"
  ZAINOD_TAG="zainod:${RUST_VERSION}-${COMMIT}"
}

# Return the engine-specific tail file for the final build stage.
tail_file_for_engine() {
  if [ "$ENGINE" = "podman" ]; then
    echo "Containerfile.tail"
  else
    echo "Dockerfile.tail"
  fi
}

# Return the command prefix needed to invoke zainod in the container.
# Podman images have ENTRYPOINT ["zainod"], so no prefix is needed.
# Docker images have no entrypoint, so the binary name must be given.
cmd_prefix_for_engine() {
  if [ "$ENGINE" = "podman" ]; then
    echo ""
  else
    echo "zainod"
  fi
}

