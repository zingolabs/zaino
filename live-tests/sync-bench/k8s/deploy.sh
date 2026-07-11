#!/usr/bin/env bash
# deploy.sh — build image, push to k3s node, run Job.
#
# Usage:
#   ./deploy.sh [block_count] [concurrency] [batch_size] [backend]
#
# Requires: podman, ssh access to k3s node, kubectl context 'zingo-infra'.

set -euo pipefail

BLOCKS="${1:-10000}"
CONC="${2:-16}"
BATCH="${3:-50}"
BACKEND="${4:-lmdb}"

CONTEXT="zingo-infra"
NS="golden-mainnet"
NODE="tekau"  # control-plane node
IMAGE="sync-bench:local"
RPC="http://zebra.golden-mainnet.svc:8232"

echo "=== Building container image ==="
podman build -t "$IMAGE" -f live-tests/sync-bench/Containerfile .

echo "=== Saving and copying to node ==="
podman save "$IMAGE" -o /tmp/sync-bench.tar
scp /tmp/sync-bench.tar "$NODE":/tmp/sync-bench.tar

echo "=== Importing image on node ==="
ssh "$NODE" "sudo k3s ctr images import /tmp/sync-bench.tar && rm /tmp/sync-bench.tar"

# Delete previous job if exists.
kubectl --context "$CONTEXT" -n "$NS" delete job sync-bench 2>/dev/null || true

# Build env section.
ENV_YAML="            - name: ZEBRA_RPC_URL
              value: \"$RPC\""
if [ "$BACKEND" = "lmdb" ]; then
  ENV_YAML="$ENV_YAML
            - name: ZAINO_DB_PATH
              value: \"/tmp/zaino-bench-db\""
fi

echo "=== Creating Job: $BLOCKS blocks, conc=$CONC, batch=$BATCH, backend=$BACKEND ==="

cat <<EOF | kubectl --context "$CONTEXT" -n "$NS" apply -f -
apiVersion: batch/v1
kind: Job
metadata:
  name: sync-bench
  namespace: $NS
spec:
  ttlSecondsAfterFinished: 600
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: bench
          image: docker.io/library/$IMAGE
          imagePullPolicy: Never
          args: ["$BLOCKS", "$CONC", "$BATCH"]
          env:
$ENV_YAML
EOF

echo ""
echo "=== Waiting for pod to start ==="
kubectl --context "$CONTEXT" -n "$NS" wait --for=condition=Ready \
  -l job-name=sync-bench pod --timeout=60s

echo ""
echo "=== Streaming logs (Ctrl-C to detach, job keeps running) ==="
kubectl --context "$CONTEXT" -n "$NS" logs -f job/sync-bench
