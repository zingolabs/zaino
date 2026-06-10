#!/usr/bin/env bash
# Print the current CONTAINER_DIR_HASH.

HASH=$(./tools/scripts/get-container-hash.sh)
echo "CONTAINER_DIR_HASH=$HASH"
