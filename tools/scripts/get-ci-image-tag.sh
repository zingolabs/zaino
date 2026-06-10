#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
# shellcheck source=tools/scripts/functions.sh
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# Accepts env vars already loaded in the calling context
main() {
  local container_hash
  container_hash=$(get_container_hash)

  local tag_vars
  tag_vars="RUST_$RUST_VERSION-ZCASH_$ZCASH_VERSION"
  tag_vars+="-ZEBRA_$ZEBRA_VERSION-CONTAINER_$container_hash"
  echo "$tag_vars"
}

main "$@"

