#!/usr/bin/env bash
# Rerun only failed tests from a previous container-test-save-failures run.
#
# Sourced as the script.main of the `container-test-retry-failures` task
# (extends `base-script`); info and err come from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

FAILURES_FILE=".failed-tests"

if [[ ! -f "$FAILURES_FILE" ]]; then
    err "No $FAILURES_FILE found. Run \
'makers container-test-save-failures' first."
    exit 1
fi

FAIL_COUNT=$(wc -l < "$FAILURES_FILE" | tr -d ' ')
if [[ "$FAIL_COUNT" -eq 0 ]]; then
    info "No failed tests to retry!"
    exit 0
fi

info "Retrying $FAIL_COUNT failed test(s)..."

# Build filter from libtest-json format: "package::binary$testname"
# Output format: (package(P) & binary(B) & test(=T)) | ...
FILTER=$(while IFS= read -r line; do
    pkg="${line%%::*}"
    rest="${line#*::}"
    bin="${rest%%\$*}"
    test="${rest#*\$}"
    echo "(package($pkg) & binary($bin) & test(=$test))"
done < "$FAILURES_FILE" | tr '\n' '|' | sed 's/|$//; s/|/ | /g')

info "Filter: $FILTER"
makers container-test -E "$FILTER" "$@"
