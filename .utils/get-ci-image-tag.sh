#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# Accepts env vars already loaded in the calling context
main() {
  local docker_hash
  docker_hash=$(get_docker_hash)
  
  local tag_vars="RUST_$RUST_VERSION-ZCASH_$ZCASH_VERSION-ZEBRA_$ZEBRA_VERSION-DOCKER_$docker_hash"
  local tag
  tag=$(echo "$tag_vars" | tr ' ' '\n' | sort | sha256sum | cut -c1-12)
  # echo "VERSIONS: $tag_vars"
  # echo "TAG: $tag"
  echo "$tag_vars"
}

main "$@"

