#!/usr/bin/env bash
# Re-resolve every package in Cargo.lock *except* the LRZ / zebra / sapling-crypto
# subgraph pinned to yanked `core2 0.3.x`.
#
# Why: `core2` 0.3.0–0.3.3 was yanked from crates.io on 2026-04-17. Upstream
# migrated to `corez` (zcash/librustzcash merge d8491512, zcash/sapling-crypto
# merge b8a81c22, both on 2026-04-17) but no release has been cut for the 0.5.x
# sapling line that zebra 6.x depends on. Until zebra bumps to sapling-crypto
# 0.6.x and librustzcash cuts new point releases, plain `cargo update` fails on
# the yanked `core2 ^0.3` requirement. This script keeps that subgraph frozen
# and updates everything else.
#
# Remove this script and the pin once new crates.io releases of the affected
# packages exist; a plain `cargo update` will then work.
set -euo pipefail

# Frozen subgraph — transitive closure of `core2 0.3.x` consumers.
FROZEN_RE='^(core2|equihash|orchard|sapling-crypto|zcash_(address|client_backend|encoding|keys|note_encryption|primitives|proofs|protocol|transparent)|zebra-.*)$'

cd "$(git rev-parse --show-toplevel)"

mapfile -t candidates < <(
  cargo metadata --format-version 1 --offline \
    | jq -r '.packages[] | "\(.name)@\(.version)"' \
    | sort -u \
    | awk -F@ -v re="$FROZEN_RE" '$1 !~ re'
)

if [[ "${1:-}" == "--dry-run" ]]; then
  printf '%s\n' "${candidates[@]}"
  exit 0
fi

args=()
for p in "${candidates[@]}"; do args+=("-p" "$p"); done
exec cargo update "${args[@]}"
