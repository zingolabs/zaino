#!/usr/bin/env bash
# Initialize named podman volumes for container builds.

# `podman volume inspect` reads podman's database, not the filesystem, so a
# volume can "exist" in the DB while its backing directory is gone (e.g. after
# a partial ~/.local/share/containers cleanup or an interrupted `podman system
# reset`). Podman then refuses to mount it with "failed to validate if host
# path is dangerous: lstat .../volumes/<vol>: no such file or directory". Guard
# on the actual Mountpoint directory, and force-recreate a stale record so the
# DB and on-disk storage stay in sync.
for vol in zaino-container-target zaino-cargo-git zaino-cargo-registry; do
    mountpoint="$(podman volume inspect --format '{{.Mountpoint}}' "$vol" 2>/dev/null)"
    if [[ -z "$mountpoint" || ! -d "$mountpoint" ]]; then
        # -z: no DB record. `! -d`: DB record present but backing dir gone;
        # rm --force drops the dangling record before we recreate it.
        podman volume rm --force "$vol" >/dev/null 2>&1 || true
        podman volume create "$vol"
        echo "Created podman volume: $vol"
    fi
done

# Pre-create host-side target/ owned by the current user. The
# container-test podman invocation bind-mounts $PWD into the container and
# then overlays the zaino-container-target volume on /.../zaino/target. If
# host-side target/ does not exist when that mount layer is applied, some
# podman/runc/buildah combinations create it under a uidmap-escaped UID
# (e.g. UID 100000), which then breaks host-side `cargo` with EACCES.
# Pre-creating it as the current user avoids that. If target/ already exists
# but is unwritable (a previous leak), recreate it: rm -rf works because the
# parent directory is owned by the current user even if target/ itself is
# not.
if [[ -e target && ! -w target ]]; then
    echo "target/ exists but is not writable; recreating to recover from \
uidmap leak..."
    rm -rf target
fi
mkdir -p target
