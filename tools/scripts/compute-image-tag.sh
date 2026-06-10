#!/usr/bin/env bash
# Compute the container image tag from the version vars.

TAG=$(./tools/scripts/get-ci-image-tag.sh)
echo "CARGO_MAKE_IMAGE_TAG=$TAG"
export CARGO_MAKE_IMAGE_TAG=$TAG
