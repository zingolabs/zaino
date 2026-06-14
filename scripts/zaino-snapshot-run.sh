#!/usr/bin/env bash
# zaino-snapshot-run — bundle config generation + a live ZainoDB + snapshot watcher.
#
# Wires three things together:
#   1. `zainod generate-config` to produce ./zainod.toml, then patches it to
#      point at the zebrad you're testing against and a storage dir you choose.
#   2. `zainod start` against that config (a live, syncing ZainoDB).
#   3. `zaino-snapshot watch` on the resulting LMDB env, so size-stepped
#      snapshots are cached as the sync grows.
#
# The ZainoDB env (the dir holding data.mdb) is derived from the config as:
#       <STORAGE>/<network>/v1      e.g. /home/$USER/.local/share/zaino/mainnet/v1
# (network dir: Mainnet->mainnet, Testnet->testnet, Regtest->regtest).
#
# Usage:
#   zaino-snapshot-run setup            Generate + patch ./zainod.toml only.
#   zaino-snapshot-run run              Setup, then start zainod + snapshot watcher.
#   zaino-snapshot-run env-path         Print the derived LMDB env dir and exit.
#
# Override defaults via environment variables:
#   NETWORK=Mainnet          Mainnet | Testnet | Regtest
#   ZEBRAD_RPC=127.0.0.1:8232  zebrad JSON-RPC listen address
#   STORAGE=/home/$USER/.local/share/zaino  Zaino storage root, persistent (data.mdb under here)
#   CONFIG=./zainod.toml     config file to generate/use
#   CACHE=/home/$USER/.cache/zaino-snap-cache  snapshot cache dir, disposable
#   SIZE_STEP_GB=1           snapshot whenever data.mdb grows past this many GB
#   POLL=30                  watcher poll interval (seconds)
#
# NOTE: write-amplification figures are storage-class specific. If tekau's disk
#       (HDD/SSD/NVMe) differs from the operator's, the absolute numbers won't
#       transfer — compare trends, or match the operator's storage class.

set -euo pipefail

# Opt-in command tracing: `TRACE=1 ./zaino-snapshot-run.sh ...` echoes each
# command to stderr. Off by default — when TRACE is unset/empty the guard is a
# protected left side of `&&`, so `set -e` does not trip and nothing changes.
[ -n "${TRACE:-}" ] && set -x

# Resolve paths relative to this script so it runs from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZAINOD="${ZAINOD:-$SCRIPT_DIR/../target/release/zainod}"
SNAPSHOT="${SNAPSHOT:-$SCRIPT_DIR/zaino-snapshot.sh}"

NETWORK="${NETWORK:-Mainnet}"
ZEBRAD_RPC="${ZEBRAD_RPC:-127.0.0.1:8232}"
# STORAGE is the long-lived ZainoDB — losing it means a full re-sync — so it
# goes under XDG_DATA_HOME (~/.local/share), NOT ~/.cache, which the XDG spec
# treats as disposable (cache-cleaners may wipe it). The snapshot CACHE below
# *is* disposable, so ~/.cache is right for that one. $USER can be empty under
# cron/systemd/sudo, so fall back to `id -un`.
STORAGE="${STORAGE:-/home/${USER:-$(id -un)}/.local/share/zaino}"
CONFIG="${CONFIG:-./zainod.toml}"
CACHE="${CACHE:-/home/${USER:-$(id -un)}/.cache/zaino-snap-cache}"
SIZE_STEP_GB="${SIZE_STEP_GB:-1}"
POLL="${POLL:-30}"

# Map the configured network to its on-disk v1 directory name.
net_dir() {
    case "$NETWORK" in
        Mainnet) echo mainnet;;
        Testnet) echo testnet;;
        Regtest) echo regtest;;
        *) echo "unknown NETWORK '$NETWORK' (use Mainnet|Testnet|Regtest)" >&2; exit 2;;
    esac
}

env_path() { echo "$STORAGE/$(net_dir)/v1"; }

# Replace a top-level `key = '...'` line in the generated TOML, anchored at line
# start so we don't touch lookalike keys (e.g. zebra_db_path vs path).
set_kv() {
    local key="$1" val="$2" file="$3"
    sed -i "s|^${key} = .*|${key} = '${val}'|" "$file"
}

cmd_setup() {
    [ -x "$ZAINOD" ] || { echo "zainod not found/executable at '$ZAINOD'" >&2; exit 1; }

    "$ZAINOD" generate-config --output "$CONFIG"

    # Point at the zebrad under test and our chosen storage dir; force the
    # fetch backend (talks JSON-RPC to zebrad — no zebra db needed).
    set_kv backend                          fetch        "$CONFIG"
    set_kv network                          "$NETWORK"   "$CONFIG"
    set_kv validator_jsonrpc_listen_address "$ZEBRAD_RPC" "$CONFIG"
    set_kv path                             "$STORAGE"   "$CONFIG"

    echo "config:    $CONFIG"
    echo "network:   $NETWORK"
    echo "zebrad:    $ZEBRAD_RPC"
    echo "storage:   $STORAGE"
    echo "env (db):  $(env_path)"
    echo "cache:     $CACHE"
}

cmd_run() {
    cmd_setup

    local env_dir; env_dir="$(env_path)"
    mkdir -p "$CACHE"

    echo "starting zainod..."
    "$ZAINOD" start --config "$CONFIG" &
    local zainod_pid=$!
    # Tear zainod down if the watcher (or this script) exits.
    trap 'kill "$zainod_pid" 2>/dev/null || true' EXIT

    # Wait for the env to materialise before watching (zainod creates it on first
    # finalised write, which only happens once it connects to zebrad and syncs).
    echo "waiting for $env_dir/data.mdb ..."
    while [ ! -f "$env_dir/data.mdb" ]; do
        kill -0 "$zainod_pid" 2>/dev/null || { echo "zainod exited before creating the db" >&2; exit 1; }
        sleep 2
    done

    echo "watching for snapshots (step ${SIZE_STEP_GB}GB, poll ${POLL}s)..."
    "$SNAPSHOT" watch "$env_dir" "$CACHE" --size-step "$SIZE_STEP_GB" --poll "$POLL"
}

case "${1:-}" in
    setup)    cmd_setup;;
    run)      cmd_run;;
    env-path) env_path;;
    *) echo "usage: $0 {setup|run|env-path}" >&2; exit 2;;
esac
