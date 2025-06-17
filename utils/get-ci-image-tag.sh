# ./utils/get-ci-image-tag.sh

#!/usr/bin/env bash
set -euo pipefail

# Accepts env vars already loaded in the calling context
main() {
  local tag_vars="RUST_$RUST_VERSION-ZCASH_$ZCASH_VERSION-ZEBRA_$ZEBRA_VERSION"
  local tag
  tag=$(echo "$tag_vars" | tr ' ' '\n' | sort | sha256sum | cut -c1-12)
  # echo "VERSIONS: $tag_vars"
  # echo "TAG: $tag"
  echo "$tag_vars"
}

main "$@"

