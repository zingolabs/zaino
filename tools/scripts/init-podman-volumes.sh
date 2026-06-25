#!/usr/bin/env bash
# Initialize named podman volumes for container builds.

for vol in zaino-container-target zaino-cargo-git zaino-cargo-registry; do
    if ! podman volume inspect "$vol" >/dev/null 2>&1; then
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
