#!/usr/bin/env bash
# deploy.sh — build, push, and run sync benchmark as a k8s Job.
#
# Usage:
#   ./deploy.sh [block_count] [concurrency] [batch_size] [source]
#
# Examples:
#   ./deploy.sh 10000 16 50                  # ReadState + LMDB (default)
#   ./deploy.sh 10000 16 50 rpc              # RPC + LMDB
#   ./deploy.sh 3410000 16 1000              # full chain
#
# Environment:
#   SKIP_BUILD=1    — skip podman build (reuse existing image)
#   SKIP_PUSH=1     — skip scp + import (image already on node)
#
# Output:
#   - Streams via kubectl logs -f
#   - Saved to ./bench-results/<timestamp>.txt
#   - Job auto-deletes after 2 hours (ttlSecondsAfterFinished)
#
# Requires: podman, ssh root@tekau, kubectl context 'zingo-infra'.

set -euo pipefail

BLOCKS="${1:-10000}"
CONC="${2:-16}"
BATCH="${3:-50}"
SOURCE="${4:-readstate}"

CONTEXT="zingo-infra"
NS="golden-mainnet"
NODE="root@tekau"
IMAGE="sync-bench:local"
RPC="http://zebra.golden-mainnet.svc:8232"
ZEBRA_PVC_PATH="/var/lib/kubelet/pods/cc3f278f-277c-4bcb-84e1-edbf8699d01d/volumes/kubernetes.io~csi/pvc-787fcbe1-bea1-49be-9daf-9953a3ea8464/mount"
RESULTS_DIR="./bench-results"

mkdir -p "$RESULTS_DIR"
RESULT_FILE="$RESULTS_DIR/$(date +%Y%m%d-%H%M%S)-${BLOCKS}b-c${CONC}-bs${BATCH}-${SOURCE}.txt"

# --- Build ---
if [ "${SKIP_BUILD:-}" != "1" ]; then
  echo "=== Building container image ==="
  podman build -t "$IMAGE" -f live-tests/sync-bench/Containerfile . 2>&1 | tail -3
else
  echo "=== Skipping build (SKIP_BUILD=1) ==="
fi

# --- Push to node ---
if [ "${SKIP_PUSH:-}" != "1" ]; then
  echo "=== Pushing image to node ==="
  podman save --format oci-archive "$IMAGE" -o /tmp/sync-bench.tar
  scp /tmp/sync-bench.tar "$NODE":/tmp/sync-bench.tar
  ssh "$NODE" "k3s ctr images import /tmp/sync-bench.tar && rm /tmp/sync-bench.tar"
else
  echo "=== Skipping push (SKIP_PUSH=1) ==="
fi

# --- Delete previous job ---
kubectl --context "$CONTEXT" -n "$NS" delete job sync-bench 2>/dev/null || true

# --- Build Job manifest ---
echo "=== Creating Job: $BLOCKS blocks, conc=$CONC, batch=$BATCH, source=$SOURCE ==="

if [ "$SOURCE" = "readstate" ]; then
  ENV_YAML="            - name: ZEBRA_STATE_DIR
              value: \"/zebra-state\"
            - name: ZAINO_DB_PATH
              value: \"/data/zaino-bench\""
  VOLUMES_YAML="      volumes:
        - name: zebra-state
          hostPath:
            path: $ZEBRA_PVC_PATH
            type: Directory
        - name: bench-data
          emptyDir:
            sizeLimit: 300Gi"
  MOUNTS_YAML="          volumeMounts:
            - name: zebra-state
              mountPath: /zebra-state
              readOnly: true
            - name: bench-data
              mountPath: /data"
  NODE_SEL="      nodeSelector:
        kubernetes.io/hostname: tekau"
else
  ENV_YAML="            - name: ZEBRA_RPC_URL
              value: \"$RPC\"
            - name: ZAINO_DB_PATH
              value: \"/data/zaino-bench\""
  VOLUMES_YAML="      volumes:
        - name: bench-data
          emptyDir:
            sizeLimit: 300Gi"
  MOUNTS_YAML="          volumeMounts:
            - name: bench-data
              mountPath: /data"
  NODE_SEL=""
fi

cat <<EOF | kubectl --context "$CONTEXT" -n "$NS" apply -f -
apiVersion: batch/v1
kind: Job
metadata:
  name: sync-bench
  namespace: $NS
spec:
  ttlSecondsAfterFinished: 7200
  activeDeadlineSeconds: 28800
  template:
    spec:
      restartPolicy: Never
$NODE_SEL
      containers:
        - name: bench
          image: localhost/$IMAGE
          imagePullPolicy: Never
          args: ["$BLOCKS", "$CONC", "$BATCH"]
          env:
$ENV_YAML
$MOUNTS_YAML
$VOLUMES_YAML
EOF

# --- Wait + stream ---
echo ""
echo "=== Waiting for pod ==="
kubectl --context "$CONTEXT" -n "$NS" wait --for=condition=Ready \
  -l job-name=sync-bench pod --timeout=120s

echo ""
echo "=== Streaming logs → $RESULT_FILE ==="
echo "=== (Ctrl-C to detach — Job keeps running) ==="
echo ""

kubectl --context "$CONTEXT" -n "$NS" logs -f job/sync-bench | tee "$RESULT_FILE"

echo ""
echo "=== Results saved to: $RESULT_FILE ==="
