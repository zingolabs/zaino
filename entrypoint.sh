#!/usr/bin/env bash

# Entrypoint for running Zaino in a container.
#
# The container MUST run as a non-root user. If started as root, the entrypoint
# exits immediately with an error.
#
# Zaino (runtime stack) is configured by a greenfield TOML (see
# `zainod generate-config`). This entrypoint bridges a small, stable set of
# container env vars into that TOML, then starts the daemon against it. Two ways
# to configure, highest priority first:
#
#   1. Mount a complete config and point ZAINO_CONFIG_FILE at it — used verbatim.
#   2. Otherwise this script synthesizes a config from the ZAINO_* vars below.

set -eo pipefail

if [[ "$(id -u)" == '0' ]]; then
  echo "ERROR: Refusing to run as root. Run this container as a non-root user." >&2
  exit 1
fi

# Container env contract (simple, stable — distinct from zainod's internal
# ZAINO_-prefixed config-rs keys, which shift with the config schema):
: "${ZAINO_VALIDATOR_JSONRPC:=127.0.0.1:8232}"   # validator JSON-RPC host:port
: "${ZAINO_VALIDATOR_COOKIE:=}"                   # validator auth cookie path (optional)
: "${ZAINO_STORE_PATH:=/app/data}"                # LMDB index directory
: "${ZAINO_STORE_MAP_SIZE_GB:=16}"                # LMDB map size (GiB)
: "${ZAINO_GRPC_LISTEN:=0.0.0.0:8137}"            # CompactTxStreamer bind (0.0.0.0 to be reachable)

# The config the daemon is started against. Either the mounted file, or the one
# synthesized here under the config dir (symlinked from ~/.config/zaino).
ZAINO_CONFIG_DIR="/app/config"
CONFIG_PATH="${ZAINO_CONFIG_FILE:-${ZAINO_CONFIG_DIR}/zainod.toml}"

# Create writable dirs the daemon needs.
for dir in "${ZAINO_STORE_PATH}" "${ZAINO_CONFIG_DIR}"; do
  [[ -z "${dir}" ]] && continue
  if ! mkdir -p "${dir}" 2>/dev/null; then
    echo "WARN: Cannot create ${dir} (read-only or permission denied), skipping" >&2
  fi
done

if [[ -n "${ZAINO_CONFIG_FILE:-}" ]]; then
  echo "Using mounted config: ${CONFIG_PATH}" >&2
elif [[ -f "${CONFIG_PATH}" ]]; then
  echo "Using existing config: ${CONFIG_PATH}" >&2
else
  echo "Synthesizing config at ${CONFIG_PATH}" >&2
  {
    cat <<EOF
[source]
jsonrpc_address = "${ZAINO_VALIDATOR_JSONRPC}"
EOF
    [[ -n "${ZAINO_VALIDATOR_COOKIE}" ]] && echo "cookie_path = \"${ZAINO_VALIDATOR_COOKIE}\""
    cat <<EOF

[store]
path = "${ZAINO_STORE_PATH}"
map_size_gb = ${ZAINO_STORE_MAP_SIZE_GB}

[serve]
grpc_listen_address = "${ZAINO_GRPC_LISTEN}"
EOF
  } > "${CONFIG_PATH}"
fi

# Contract vars share zainod's ZAINO_ env-override prefix: left exported, each
# would parse as an unknown config key and abort startup (deny_unknown_fields).
unset ZAINO_VALIDATOR_JSONRPC ZAINO_VALIDATOR_COOKIE ZAINO_STORE_PATH \
  ZAINO_STORE_MAP_SIZE_GB ZAINO_GRPC_LISTEN ZAINO_CONFIG_FILE

# `start` still layers zainod's own ZAINO_-prefixed keys (e.g. ZAINO_INDEXER__BATCH_SIZE)
# over the file for scalar fields.
exec zainod start --config "${CONFIG_PATH}"
