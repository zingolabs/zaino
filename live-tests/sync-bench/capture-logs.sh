#!/usr/bin/env bash
# capture-logs.sh — stream k8s Job logs into a local container, persist to volume.
#
# Usage:
#   ./capture-logs.sh [job-name] [namespace]
#
# The logs are written to ./k8s/logs/<job-name>.jsonl (host-mounted).
# When the job finishes (or you ctrl-c), parse with:
#   python parse-logs.py k8s/logs/<job-name>.jsonl
#
# Compare two runs:
#   python parse-logs.py --compare k8s/logs/run-a.jsonl k8s/logs/run-b.jsonl

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

JOB="${1:-sync-bench}"
NS="${2:-golden-mainnet}"
LOG_DIR="$SCRIPT_DIR/k8s/logs"
LOG_FILE="$LOG_DIR/$JOB.jsonl"

mkdir -p "$LOG_DIR"

echo "Capturing logs from job/$JOB -n $NS -> $LOG_FILE"
echo "Press Ctrl-C to stop (logs are flushed continuously)."
echo ""

kubectl logs -f "job/$JOB" -n "$NS" 2>&1 | tee "$LOG_FILE"
