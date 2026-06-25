#!/usr/bin/env bash
# Ensure the image exists locally, building it if needed.
#
# Sourced as the script.main of the `ensure-image-exists` task, which extends
# `base-script`; TAG, IMAGE_NAME, and the info/warn/err helpers come from the
# base-script pre-script (tools/scripts/base-script-pre.sh).

if ! podman image inspect "${IMAGE_NAME}:${TAG}" > /dev/null 2>&1; then
  info "Image not found locally. Building..."
  makers build-image
else
  info "Image ${IMAGE_NAME}:${TAG} already exists locally."
fi
