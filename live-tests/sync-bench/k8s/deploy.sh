#!/usr/bin/env bash
# deploy.sh — build, push, and run the sync-bench Job on k8s.
#
# Usage:
#   ./deploy.sh [block_count] [concurrency] [batch_size]
#
# Environment:
#   K8S_NODE        — target node for image push (default: tekau)
#   JOB_NAME        — k8s Job name (default: sync-bench)
#   JOB_NAMESPACE   — k8s namespace (default: golden-mainnet)
#   RUST_LOG        — tracing filter (default: zaino_sync=trace)
#   ZAINO_LOG_JSON  — set to 1 for JSON log output
#   SKIP_BUILD      — set to 1 to skip image build
#   SKIP_PUSH       — set to 1 to skip image push
#   SOURCE          — "readstate" (default) or "rpc"
#
# Examples:
#   ./deploy.sh 3410000 16 1000                # full chain, ReadState+LMDB
#   JOB_NAME=bench-v2 ./deploy.sh 100000 16 500  # named job alongside others
#   SKIP_BUILD=1 SKIP_PUSH=1 ./deploy.sh 10000   # reuse existing image

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

BLOCK_COUNT="${1:-100}"
CONCURRENCY="${2:-16}"
BATCH_SIZE="${3:-50}"
SOURCE="${SOURCE:-readstate}"

NODE="${K8S_NODE:-tekau}"
NS="${JOB_NAMESPACE:-golden-mainnet}"
COMMIT_SHORT=$(git -C "$REPO_ROOT" rev-parse --short=7 HEAD)
JOB_TAG="${JOB_TAG:-}"
JOB="${JOB_NAME:-bench-${COMMIT_SHORT}${JOB_TAG:+-$JOB_TAG}}"
IMAGE="sync-bench:local"
LOG="${RUST_LOG:-sync_bench=info,zaino_sync=trace}"
LOG_JSON="${ZAINO_LOG_JSON:-1}"
OTEL_ENDPOINT="${OTEL_EXPORTER_OTLP_ENDPOINT:-http://tempo.monitoring.svc:4317}"

# ── Build ──────────────────────────────────────────────────────
if [[ "${SKIP_BUILD:-}" != "1" ]]; then
  echo "▸ building image…"
  podman build -t "$IMAGE" -f "$SCRIPT_DIR/../Containerfile" "$REPO_ROOT"
fi

# ── Push to node ───────────────────────────────────────────────
if [[ "${SKIP_PUSH:-}" != "1" ]]; then
  echo "▸ pushing image to $NODE…"
  podman save "$IMAGE" | ssh "root@$NODE" 'ctr -n k8s.io images import -'
fi

# ── Zebra state volume path ───────────────────────────────────
# Resolve the PVC-backed hostPath for Zebra's RocksDB state.
ZEBRA_HOSTPATH=$(kubectl get pvc zebra-state -n "$NS" -o jsonpath='{.spec.volumeName}' 2>/dev/null || true)
if [[ -n "$ZEBRA_HOSTPATH" ]]; then
  ZEBRA_HOSTPATH=$(kubectl get pv "$ZEBRA_HOSTPATH" -o jsonpath='{.spec.csi.volumeHandle}' 2>/dev/null || true)
fi
# Fallback: use the path from the existing sync-bench Job if available.
if [[ -z "$ZEBRA_HOSTPATH" ]]; then
  ZEBRA_HOSTPATH=$(kubectl get job sync-bench -n "$NS" -o jsonpath='{.spec.template.spec.volumes[?(@.name=="zebra-state")].hostPath.path}' 2>/dev/null || true)
fi
if [[ -z "$ZEBRA_HOSTPATH" ]]; then
  echo "⚠ could not resolve zebra-state volume path; set it manually in the Job spec" >&2
  exit 1
fi

# ── Source-specific env/volumes ────────────────────────────────
if [[ "$SOURCE" == "rpc" ]]; then
  ENV_YAML=$(cat <<ENVEOF
        - name: ZEBRA_RPC_URL
          value: "http://zebra.${NS}.svc:8232"
        - name: ZAINO_DB_PATH
          value: "/data/zaino-bench"
        - name: RUST_LOG
          value: "$LOG"
        - name: ZAINO_LOG_JSON
          value: "$LOG_JSON"
        - name: OTEL_EXPORTER_OTLP_ENDPOINT
          value: "$OTEL_ENDPOINT"
ENVEOF
)
  VOLUME_MOUNTS_YAML=$(cat <<VMEOF
        - mountPath: /data
          name: bench-data
VMEOF
)
  VOLUMES_YAML=$(cat <<VEOF
      - emptyDir:
          sizeLimit: 300Gi
        name: bench-data
VEOF
)
else
  ENV_YAML=$(cat <<ENVEOF
        - name: ZEBRA_STATE_DIR
          value: "/zebra-state"
        - name: ZAINO_DB_PATH
          value: "/data/zaino-bench"
        - name: RUST_LOG
          value: "$LOG"
        - name: ZAINO_LOG_JSON
          value: "$LOG_JSON"
        - name: OTEL_EXPORTER_OTLP_ENDPOINT
          value: "$OTEL_ENDPOINT"
ENVEOF
)
  VOLUME_MOUNTS_YAML=$(cat <<VMEOF
        - mountPath: /zebra-state
          name: zebra-state
          readOnly: true
        - mountPath: /data
          name: bench-data
VMEOF
)
  VOLUMES_YAML=$(cat <<VEOF
      - hostPath:
          path: "$ZEBRA_HOSTPATH"
          type: Directory
        name: zebra-state
      - emptyDir:
          sizeLimit: 300Gi
        name: bench-data
VEOF
)
fi

# ── Apply Job ──────────────────────────────────────────────────
echo "▸ creating Job $JOB in $NS (${BLOCK_COUNT} blocks, concurrency=$CONCURRENCY, batch=$BATCH_SIZE)"

kubectl apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $JOB
  namespace: $NS
spec:
  activeDeadlineSeconds: 28800
  ttlSecondsAfterFinished: 7200
  template:
    spec:
      restartPolicy: Never
      nodeSelector:
        kubernetes.io/hostname: $NODE
      containers:
      - name: bench
        image: localhost/$IMAGE
        imagePullPolicy: Never
        args: ["$BLOCK_COUNT", "$CONCURRENCY", "$BATCH_SIZE"]
        env:
$ENV_YAML
        volumeMounts:
$VOLUME_MOUNTS_YAML
      volumes:
$VOLUMES_YAML
EOF

echo "▸ Job submitted. Watch with:"
echo "    kubectl logs -f job/$JOB -n $NS"
