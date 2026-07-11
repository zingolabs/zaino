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
# Requires: kubectl context 'zingo-infra', namespace 'golden-mainnet'.
# The binary is built locally (release) and copied to an archlinux pod.

set -euo pipefail

BLOCKS="${1:-10000}"
CONC="${2:-16}"
BATCH="${3:-50}"
BACKEND="${4:-memory}"

CONTEXT="zingo-infra"
NS="golden-mainnet"
POD="sync-bench-$$"
RPC="http://zebra.golden-mainnet.svc:8232"
BIN="target/release/sync-headers"

echo "=== Building release binary ==="
cargo build -p sync-bench --release --quiet

echo "=== Deploying pod ${POD} ==="
kubectl --context "$CONTEXT" -n "$NS" run "$POD" \
  --image=archlinux:latest \
  --restart=Never \
  --command -- sleep 3600

# Wait for pod to be running.
kubectl --context "$CONTEXT" -n "$NS" wait --for=condition=Ready "pod/$POD" --timeout=30s

echo "=== Copying binary ==="
kubectl --context "$CONTEXT" -n "$NS" cp "$BIN" "$POD:/tmp/sync-headers"
kubectl --context "$CONTEXT" -n "$NS" exec "$POD" -- chmod +x /tmp/sync-headers

# Build env vars.
ENV="ZEBRA_RPC_URL=$RPC"
if [ "$BACKEND" = "lmdb" ]; then
  ENV="$ENV ZAINO_DB_PATH=/tmp/zaino-bench-db"
fi

echo "=== Running: $BLOCKS blocks, conc=$CONC, batch=$BATCH, backend=$BACKEND ==="
kubectl --context "$CONTEXT" -n "$NS" exec "$POD" -- \
  env $ENV /tmp/sync-headers "$BLOCKS" "$CONC" "$BATCH"

echo ""
echo "=== Cleaning up pod ${POD} ==="
kubectl --context "$CONTEXT" -n "$NS" delete pod "$POD" --force --grace-period=0 2>/dev/null || true

echo "=== Done ==="
