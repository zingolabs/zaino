#!/usr/bin/env bash
# Push the image to the registry.
#
# Sourced as the script.main of the `push-image` task (extends `base-script`);
# TAG, IMAGE_NAME, and info come from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "Pushing image: ${IMAGE_NAME}:${TAG}"

podman push "${IMAGE_NAME}:${TAG}"
