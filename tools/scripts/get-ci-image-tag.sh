#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
# shellcheck source=tools/scripts/functions.sh
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# The image is a pure Rust build environment (no validator binaries), so the
# tag needs only the toolchain version and a content hash of the build context
# (Containerfile). RUST_VERSION is expected in the calling context.
main() {
  local container_hash
  container_hash=$(get_container_hash)
  echo "RUST_$RUST_VERSION-CONTAINER_$container_hash"
}

main "$@"

