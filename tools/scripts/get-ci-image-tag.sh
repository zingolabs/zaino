#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
# shellcheck source=tools/scripts/functions.sh
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# The image is a pure Rust build environment (no validator binaries) fully
# described by the Containerfile, toolchain pin included — so a content hash of
# the build context is the whole tag.
main() {
  local container_hash
  container_hash=$(get_container_hash)
  echo "CONTAINER_$container_hash"
}

main "$@"

