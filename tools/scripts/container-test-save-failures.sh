#!/usr/bin/env bash
# Run container-test and save failed test names for later retry.
#
# Sourced as the script.main of the `container-test-save-failures` task
# (extends `base-script`); info and warn come from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -uo pipefail

FAILURES_FILE=".failed-tests"

info "Running container-test with failure tracking..."

# Run container-test with libtest-json, tee output for parsing
makers container-test \
    --no-fail-fast --message-format libtest-json "$@" 2>&1 \
    | tee /tmp/nextest-output.json || true

# Extract failed test names
grep '"event":"failed"' /tmp/nextest-output.json 2>/dev/null \
    | jq -r '.name // empty' | sort -u > "$FAILURES_FILE"

FAIL_COUNT=$(wc -l < "$FAILURES_FILE" | tr -d ' ')

if [[ "$FAIL_COUNT" -gt 0 ]]; then
    warn "💾 Saved $FAIL_COUNT failed test(s) to $FAILURES_FILE"
    cat "$FAILURES_FILE"
    echo ""
    info "Run 'makers container-test-retry-failures' to rerun them"
else
    info "✅ All tests passed!"
    rm -f "$FAILURES_FILE"
fi
