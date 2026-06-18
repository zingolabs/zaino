#!/usr/bin/env bash
# Run both integration-test workspaces and print a combined pass/fail summary.
#
# Used as the script of the `integration-test` task. Runs walletless-tests then
# wallet-tests (each in its own CI container via its own makers task), tees each
# run's output, parses the nextest summary line, and aggregates the totals.
#
# Unlike a cargo-make `dependencies` list (which is fail-fast), this runs BOTH
# workspaces even when the first fails, so the summary reflects the whole suite.
# It then exits non-zero if either workspace failed, so CI still catches it.

set -uo pipefail

# parse_summary <logfile> -> echoes "run passed failed skipped" (zeros if absent).
# nextest prints e.g.:
#   Summary [ 73.207s] 8 tests run: 8 passed (2 slow), 2 skipped
#   Summary [510.718s] 29 tests run: 23 passed (14 slow), 6 failed, 2 skipped
#   Summary [  1.795s] 1 test run: 0 passed, 1 failed, 114 skipped   (singular)
parse_summary() {
    local log="$1" line
    # Strip ANSI, take the last "N test(s) run:" line nextest emitted.
    line=$(perl -pe 's/\e\[[0-9;]*m//g' "$log" 2>/dev/null | grep -E '[0-9]+ tests? run:' | tail -1)
    local run passed failed skipped
    run=$(printf '%s' "$line" | perl -ne 'print $1 if /(\d+) tests? run:/')
    passed=$(printf '%s' "$line" | perl -ne 'print $1 if /(\d+) passed/')
    failed=$(printf '%s' "$line" | perl -ne 'print $1 if /(\d+) failed/')
    skipped=$(printf '%s' "$line" | perl -ne 'print $1 if /(\d+) skipped/')
    printf '%s %s %s %s' "${run:-0}" "${passed:-0}" "${failed:-0}" "${skipped:-0}"
}

WL_LOG=$(mktemp)
WT_LOG=$(mktemp)
trap 'rm -f "$WL_LOG" "$WT_LOG"' EXIT

echo ">>> integration-test: running walletless-tests workspace"
makers walletless-integration-test 2>&1 | tee "$WL_LOG"
wl_rc=${PIPESTATUS[0]}

echo ">>> integration-test: running wallet-tests workspace"
makers wallet-integration-test 2>&1 | tee "$WT_LOG"
wt_rc=${PIPESTATUS[0]}

read -r wl_run wl_pass wl_fail wl_skip <<<"$(parse_summary "$WL_LOG")"
read -r wt_run wt_pass wt_fail wt_skip <<<"$(parse_summary "$WT_LOG")"

echo ""
echo "================== integration-test summary =================="
printf '  %-18s %4s run, %4s passed, %4s failed, %4s skipped\n' "walletless-tests:" "$wl_run" "$wl_pass" "$wl_fail" "$wl_skip"
printf '  %-18s %4s run, %4s passed, %4s failed, %4s skipped\n' "wallet-tests:" "$wt_run" "$wt_pass" "$wt_fail" "$wt_skip"
printf '  %-18s %4s run, %4s passed, %4s failed, %4s skipped\n' "TOTAL:" \
    "$((wl_run + wt_run))" "$((wl_pass + wt_pass))" "$((wl_fail + wt_fail))" "$((wl_skip + wt_skip))"
echo "=============================================================="

# A workspace that errored without producing a summary line likely failed to
# build; call it out so the zeros above aren't read as "all clear".
if [ "$wl_rc" -ne 0 ] && [ "$wl_run" -eq 0 ]; then
    echo "  warning: walletless-tests produced no nextest summary (build failure?) — see output above."
fi
if [ "$wt_rc" -ne 0 ] && [ "$wt_run" -eq 0 ]; then
    echo "  warning: wallet-tests produced no nextest summary (build failure?) — see output above."
fi

# Fail the umbrella task if either workspace failed.
if [ "$wl_rc" -ne 0 ] || [ "$wt_rc" -ne 0 ]; then
    exit 1
fi
