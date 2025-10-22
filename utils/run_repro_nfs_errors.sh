#!/bin/bash

# Counters
pass_count=0
timeout_112_count=0
run_count=0

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo "=========================================="
echo "Starting test loop for repro_nonf"
echo "=========================================="
echo ""

while true; do
    ((run_count++))

    echo -e "${BLUE}[Run #${run_count}]${NC} Running test..."

    # Run the test and capture output
    output=$(cargo nextest run --package integration-tests --test chain_cache repro_nonf 2>&1)
    exit_code=$?

    # Check for pass
    if [ $exit_code -eq 0 ]; then
        ((pass_count++))
        echo -e "${GREEN}[Run #${run_count}]${NC} ✓ PASSED"
        echo -e "  Pass count: ${pass_count}, Timeout count: ${timeout_112_count}"
        echo ""
        continue
    fi

    # Check for "attempt to subtract with overflow" - this is our target failure
    if echo "$output" | grep -q "attempt to subtract with overflow"; then
        echo -e "${RED}[Run #${run_count}]${NC} ✗ FOUND TARGET FAILURE: attempt to subtract with overflow"
        echo ""
        echo "=========================================="
        echo "FINAL RESULTS:"
        echo "=========================================="
        echo "Total runs: ${run_count}"
        echo "Passes: ${pass_count}"
        echo "Timeout (height >= 112) failures: ${timeout_112_count}"
        echo "Final failure: attempt to subtract with overflow"
        echo ""
        echo "Full output of final run:"
        echo "----------------------------------------"
        echo "$output"
        exit 0
    fi

    # Check for "timeout waiting for height >= 112"
    if echo "$output" | grep -q "timeout waiting for height >= 112"; then
        ((timeout_112_count++))
        echo -e "${YELLOW}[Run #${run_count}]${NC} ✗ TIMEOUT (height >= 112)"
        echo -e "  Pass count: ${pass_count}, Timeout count: ${timeout_112_count}"
        echo ""
        continue
    fi

    # If we get here, it's an unexpected failure - stop and report
    echo -e "${RED}[Run #${run_count}]${NC} ✗ UNEXPECTED FAILURE"
    echo ""
    echo "=========================================="
    echo "UNEXPECTED FAILURE - STOPPING"
    echo "=========================================="
    echo "Total runs: ${run_count}"
    echo "Passes: ${pass_count}"
    echo "Timeout (height >= 112) failures: ${timeout_112_count}"
    echo ""
    echo "Full output of failing run:"
    echo "----------------------------------------"
    echo "$output"
    exit 1
done
