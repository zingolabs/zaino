#!/usr/bin/env bash
# Shared utility functions for build scripts

get_container_hash() {
  local git_root
  git_root=$(git rev-parse --show-toplevel)
  cd "$git_root" || return 1
  git ls-tree HEAD integration-tests/test_environment \
    | git hash-object --stdin \
    | cut -c1-14
}

# Resolve a zcash-devtool ref (branch/tag) to its commit SHA. The image tag
# embeds the resolved SHA — not the ref name — so a moving branch like
# `add_regtest` changes the tag automatically whenever its HEAD advances, and
# `ensure-image-exists` stops reusing a stale baked binary. Both the tag side
# (get-ci-image-tag.sh) and the build side (build-image.sh) call this so they
# agree on the SHA. An empty `git ls-remote` result means the ref is already a
# SHA (ls-remote matches ref names, not commits), so it is returned unchanged.
resolve_devtool_rev() {
  local ref="$1" rev
  rev=$(git ls-remote https://github.com/zingolabs/zcash-devtool "$ref" 2>/dev/null | awk 'NR==1{print $1}')
  echo "${rev:-$ref}"
}