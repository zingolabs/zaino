#!/usr/bin/env bash
# Validate that nextest targets match the CI workflow matrix.
#
# Sourced as the script.main of the `validate-test-targets` task (extends
# `base-script`); info and warn come from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "🔍 Validating test targets between nextest and CI workflow..."

# Extract nextest targets with non-empty testcases.
# The `ci` profile lives in integration-tests/.config/nextest.toml, so use
# --manifest-path.
info "Extracting targets from nextest..."
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

# Extract CI matrix partition values
info "Extracting targets from CI workflow..."
CI_TARGETS=$(mktemp)
lq -r '.jobs.test.strategy.matrix.partition[]' \
  < .github/workflows/ci.yml | sort > "$CI_TARGETS"

# Compare the lists
info "Comparing target lists..."

MISSING_IN_CI=$(mktemp)
EXTRA_IN_CI=$(mktemp)

# Find targets in nextest but not in CI
comm -23 "$NEXTEST_TARGETS" "$CI_TARGETS" > "$MISSING_IN_CI"

# Find targets in CI but not in nextest (or with empty testcases)
comm -13 "$NEXTEST_TARGETS" "$CI_TARGETS" > "$EXTRA_IN_CI"

# Display results
if [[ ! -s "$MISSING_IN_CI" && ! -s "$EXTRA_IN_CI" ]]; then
    info "✅ All test targets are synchronized!"
    echo "Nextest targets ($(wc -l < "$NEXTEST_TARGETS")):"
    sed 's/^/  - /' "$NEXTEST_TARGETS"
else
    warn "❌ Test target synchronization issues found:"

    if [[ -s "$MISSING_IN_CI" ]]; then
        echo ""
        warn "📋 Targets with tests missing from CI matrix \
($(wc -l < "$MISSING_IN_CI")):"
        sed 's/^/  - /' "$MISSING_IN_CI"
    fi

    if [[ -s "$EXTRA_IN_CI" ]]; then
        echo ""
        warn "🗑️  Targets in CI matrix with no tests \
($(wc -l < "$EXTRA_IN_CI")):"
        sed 's/^/  - /' "$EXTRA_IN_CI"
    fi

    echo ""
    info "💡 To automatically update the CI workflow, run:"
    info "   makers update-test-targets"
fi

# Cleanup temp files
rm "$NEXTEST_TARGETS" "$CI_TARGETS" "$MISSING_IN_CI" "$EXTRA_IN_CI"
