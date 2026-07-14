#!/usr/bin/env bash
# bench.sh — quick benchmark without rebuilding the image.
#
# Assumes the image is already on the node (run deploy.sh first).
#
# Usage:
#   ./bench.sh [block_count] [concurrency] [batch_size] [source]
#
# Examples:
#   ./bench.sh 10000 16 50              # ReadState (default)
#   ./bench.sh 10000 16 50 rpc          # RPC
#   ./bench.sh 3410000 16 1000          # full chain

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SKIP_BUILD=1 SKIP_PUSH=1 exec "$SCRIPT_DIR/deploy.sh" "$@"
