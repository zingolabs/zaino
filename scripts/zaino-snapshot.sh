#!/usr/bin/env bash
# zaino-snapshot — local ZainoDB snapshot cache (first draft, single host).
#
# Produces consistent snapshots of a live, syncing ZainoDB into a local cache
# using LMDB's own hot backup (mdb_copy), and restores the latest cached
# snapshot to bootstrap/resume a sync. All local filesystem; no network.
#
# The ZainoDB "env" is the directory holding data.mdb (+ ephemeral lock.mdb),
# e.g.  <storage>/<network>/v1
#
# Usage:
#   zaino-snapshot snapshot <live_env> <cache_dir>
#       One consistent snapshot of <live_env> into <cache_dir>; update 'latest'.
#
#   zaino-snapshot watch <live_env> <cache_dir> --size-step <GB> [--poll <secs>]
#       Snapshot whenever data.mdb's on-disk size grows past the next <GB> step.
#       --size-step is required (no default); 6 is a reasonable start (~6 GB,
#       from early experimentation).
#
#   zaino-snapshot restore <cache_dir> <dest_env>
#       Copy the latest cached snapshot into <dest_env> (refuses to overwrite).
#
# Env:
#   SNAP_KEEP=2   retain only the N newest snapshots (rotation; must be >= 1).
#
# mdb_copy is a full, consistent copy of only the used pages — non-compact, so
# the live free-page/B-tree structure is preserved (faithful for write-cost
# benchmarking). Add -c below if you want a smaller, defragmented image instead.

set -euo pipefail

# Actual allocated bytes of the (sparse) data file — the real data size, not
# the 384GB map_size reservation.
used_bytes() { du -sB1 "$1/data.mdb" | cut -f1; }

cmd_snapshot() {
    local live="$1" cache="$2"
    [ -f "$live/data.mdb" ] || { echo "no data.mdb in '$live'" >&2; exit 1; }
    mkdir -p "$cache"
    local bytes ts dest tmp
    bytes=$(used_bytes "$live")
    ts=$(date +%Y%m%dT%H%M%S)
    dest="$cache/snap_${ts}_${bytes}"
    tmp="$cache/.tmp_${ts}_$$"
    mkdir -p "$tmp"
    # LMDB consistent hot copy of the live env. On failure, remove the partial
    # copy so a failed write doesn't leave a disk-eating temp behind (set -e
    # would otherwise abort with it in place).
    if ! mdb_copy "$live" "$tmp"; then
        rm -rf "$tmp"; echo "mdb_copy failed for '$live'" >&2; exit 1
    fi
    mv "$tmp" "$dest" || { rm -rf "$tmp"; echo "snapshot publish failed" >&2; exit 1; }
    ln -sfn "$(basename "$dest")" "$cache/latest"
    printf '%s\t%s\t%s\n' "$ts" "$bytes" "$dest" >> "$cache/manifest.tsv"
    echo "snapshot: $dest (${bytes} bytes)"

    # Retention: keep only the 2 newest snapshots. Pruning runs AFTER the new one
    # above is safely published (set -e aborts earlier on a failed write), so a
    # failed write never drops below the existing 2 — there are always 2, never
    # more. A crash between publish and prune leaves a harmless extra (>=2) that
    # the next snapshot prunes. Bounds disk to ~2 DBs even though the real DB
    # size is unknown. (snap_<ts>_* names sort chronologically, oldest first.)
    local keep="${SNAP_KEEP:-2}" i
    [[ "$keep" =~ ^[1-9][0-9]*$ ]] || { echo "SNAP_KEEP must be a positive integer (got '$keep')" >&2; exit 2; }
    shopt -s nullglob
    local -a snaps=("$cache"/snap_*)
    shopt -u nullglob
    for (( i = 0; i < ${#snaps[@]} - keep; i++ )); do
        if rm -rf "${snaps[i]}"; then
            echo "pruned old snapshot: ${snaps[i]}"
        else
            echo "warning: could not prune ${snaps[i]}" >&2
        fi
    done
}

cmd_watch() {
    local live="$1" cache="$2"; shift 2
    local step_gb="" poll=30
    while [ $# -gt 0 ]; do
        case "$1" in
            --size-step) step_gb="$2"; shift 2;;
            --poll)      poll="$2";    shift 2;;
            *) echo "unknown arg '$1'" >&2; exit 2;;
        esac
    done
    if [ -z "$step_gb" ]; then
        echo "watch requires --size-step <GB>; there is no default." >&2
        echo "Suggested starting point: --size-step 6  (~6 GB, from early experimentation)." >&2
        exit 2
    fi
    local step=$(( step_gb * 1024 * 1024 * 1024 ))
    local cur next
    cur=$(used_bytes "$live")
    next=$(( (cur / step + 1) * step ))
    echo "watching '$live'; next snapshot at $next bytes (step ${step_gb}GB, poll ${poll}s)"
    while true; do
        cur=$(used_bytes "$live")
        if [ "$cur" -ge "$next" ]; then
            cmd_snapshot "$live" "$cache"
            next=$(( (cur / step + 1) * step ))
        fi
        sleep "$poll"
    done
}

cmd_restore() {
    local cache="$1" dest="$2"
    local src="$cache/latest"
    [ -e "$src/data.mdb" ] || { echo "no 'latest' snapshot in '$cache'" >&2; exit 1; }
    [ -e "$dest/data.mdb" ] && { echo "refusing: '$dest' already has data.mdb" >&2; exit 1; }
    mkdir -p "$dest"
    cp --sparse=always "$src/data.mdb" "$dest/data.mdb"   # lock.mdb recreated on open
    echo "restored $(readlink -f "$src") -> $dest"
}

case "${1:-}" in
    snapshot) shift; cmd_snapshot "$@";;
    watch)    shift; cmd_watch    "$@";;
    restore)  shift; cmd_restore  "$@";;
    *) echo "usage: $0 {snapshot|watch|restore} ..." >&2; exit 2;;
esac
