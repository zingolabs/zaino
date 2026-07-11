#!/usr/bin/env bash
# bench.sh — build, deploy, and run sync-headers benchmark in-cluster.
#
# Usage:
#   ./bench.sh [block_count] [concurrency] [batch_size] [backend]
#
# Examples:
#   ./bench.sh 10000 16 50           # in-memory
#   ./bench.sh 10000 16 50 lmdb      # LMDB
#   ./bench.sh 100000 32 100 lmdb    # big run
#
# Output:
#   - Streams to terminal via `kubectl logs -f`
#   - Saved to ./bench-results/<timestamp>.txt
#
# Requires: kubectl context 'zingo-infra', namespace 'golden-mainnet'.

set -euo pipefail

BLOCKS="${1:-10000}"
CONC="${2:-16}"
BATCH="${3:-50}"
BACKEND="${4:-memory}"

CONTEXT="zingo-infra"
NS="golden-mainnet"
JOB="sync-bench-$(date +%s)"
RPC="http://zebra.golden-mainnet.svc:8232"
BIN="target/release/sync-headers"
RESULTS_DIR="./bench-results"

mkdir -p "$RESULTS_DIR"
RESULT_FILE="$RESULTS_DIR/$(date +%Y%m%d-%H%M%S)-${BLOCKS}b-c${CONC}-bs${BATCH}-${BACKEND}.txt"

echo "=== Building release binary ==="
cargo build -p sync-bench --release --quiet 2>&1 | grep -v "^$" || true

echo "=== Deploying pod ${JOB} ==="
kubectl --context "$CONTEXT" -n "$NS" run "$JOB" \
  --image=archlinux:latest \
  --restart=Never \
  --command -- sleep 3600

kubectl --context "$CONTEXT" -n "$NS" wait --for=condition=Ready "pod/$JOB" --timeout=30s

echo "=== Copying binary ==="
kubectl --context "$CONTEXT" -n "$NS" cp "$BIN" "$JOB:/tmp/sync-headers"
kubectl --context "$CONTEXT" -n "$NS" exec "$JOB" -- chmod +x /tmp/sync-headers

# Build env vars.
ENV="ZEBRA_RPC_URL=$RPC"
if [ "$BACKEND" = "lmdb" ]; then
  ENV="$ENV ZAINO_DB_PATH=/tmp/zaino-bench-db"
fi

echo "=== Running: $BLOCKS blocks, conc=$CONC, batch=$BATCH, backend=$BACKEND ==="
echo "=== Results will be saved to: $RESULT_FILE ==="
echo ""

# Run and tee output to both terminal and file.
kubectl --context "$CONTEXT" -n "$NS" exec "$JOB" -- \
  env $ENV /tmp/sync-headers "$BLOCKS" "$CONC" "$BATCH" \
  2>&1 | tee "$RESULT_FILE"

echo ""
echo "=== Cleaning up pod ${JOB} ==="
kubectl --context "$CONTEXT" -n "$NS" delete pod "$JOB" --force --grace-period=0 2>/dev/null || true

echo ""
echo "=== Results saved to: $RESULT_FILE ==="
