#!/usr/bin/env bash
# Update the CI workflow matrix to match nextest targets.
#
# Sourced as the script.main of the `update-test-targets` task (extends
# `base-script`); info comes from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "🔧 Updating CI workflow matrix to match nextest targets..."

# Extract nextest targets with non-empty testcases.
# The `ci` profile lives in integration-tests/.config/nextest.toml, so use
# --manifest-path.
info "Extracting current nextest targets..."
NEXTEST_TARGETS=$(mktemp)
cargo nextest list \
  --manifest-path integration-tests/Cargo.toml \
  --profile ci -T json-pretty \
  | jq -r '
      .["rust-suites"]
      | to_entries[]
      | select(.value.testcases | length > 0)
      | .key
    ' \
  | sort > "$NEXTEST_TARGETS"

echo "Found $(wc -l < "$NEXTEST_TARGETS") targets with tests:"
sed 's/^/  - /' "$NEXTEST_TARGETS"

# Update only the partition array using sed to preserve formatting.
# First, create the new partition list in the exact format we need.
NEW_PARTITION_LINES=$(mktemp)
while IFS= read -r target; do
    echo "          - \"${target}\""
done < "$NEXTEST_TARGETS" > "$NEW_PARTITION_LINES"

# Use sed to replace just the partition array section.
# Find the partition: line and replace everything until the next
# non-indented item.
sed -i '/^[[:space:]]*partition:/,/^[[:space:]]*[^[:space:]-]/{
    /^[[:space:]]*partition:/!{
        /^[[:space:]]*[^[:space:]-]/!d
    }
}' .github/workflows/ci.yml

# Now insert the new partition lines after the "partition:" line
sed -i "/^[[:space:]]*partition:$/r $NEW_PARTITION_LINES" \
  .github/workflows/ci.yml

rm "$NEW_PARTITION_LINES"

info "✅ CI workflow updated successfully!"

# Show what was changed using git diff
echo ""
info "Changes made:"
git diff --no-index /dev/null .github/workflows/ci.yml 2>/dev/null \
  | grep -e "^[+-].*partition" \
         -e "^[+-].*integration-tests" \
         -e "^[+-].*zaino" \
         -e "^[+-].*zainod" \
  || git diff .github/workflows/ci.yml \
  || echo "No changes detected"

# Cleanup temp files
rm "$NEXTEST_TARGETS"
