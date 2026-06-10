#!/usr/bin/env bash
# Pull the CI image from the registry.
#
# Sourced as the script.main of the `pull-ci-image` task (extends
# `base-script`); TAG, IMAGE_NAME, and info/err come from the base-script
# pre-script (tools/scripts/base-script-pre.sh).

info "Pulling ${IMAGE_NAME}:${TAG} from registry..."
if podman pull "${IMAGE_NAME}:${TAG}"; then
  info "Image ${IMAGE_NAME}:${TAG} pulled successfully."
else
  err "Image ${IMAGE_NAME}:${TAG} not found on registry."
  exit 1
fi
