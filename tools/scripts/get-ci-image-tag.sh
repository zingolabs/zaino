#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
# shellcheck source=tools/scripts/functions.sh
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# Accepts env vars already loaded in the calling context
main() {
  local container_hash
  container_hash=$(get_container_hash)

  # Content-address the devtool component: embed the resolved commit SHA so the
  # tag tracks the branch HEAD instead of the (constant) ref name.
  local devtool_rev
  devtool_rev=$(resolve_devtool_rev "$DEVTOOL_VERSION")

  local tag_vars
  tag_vars="RUST_$RUST_VERSION-ZCASH_$ZCASH_VERSION"
  tag_vars+="-ZEBRA_$ZEBRA_VERSION-DEVTOOL_${devtool_rev:0:12}"
  tag_vars+="-CONTAINER_$container_hash"
  echo "$tag_vars"
}

main "$@"

