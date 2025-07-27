#!/bin/bash
set -euo pipefail

# Get version environment variables
RUST_VERSION="${RUST_VERSION:?RUST_VERSION not set}"
ZCASH_VERSION="${ZCASH_VERSION:?ZCASH_VERSION not set}"
ZEBRA_VERSION="${ZEBRA_VERSION:?ZEBRA_VERSION not set}"

# Get the git hash of Dockerfile.ci (first 14 characters)
CI_DOCKERFILE_VERSION=$(git hash-object Dockerfile.ci | head -c 14)

# Format and output the tag
echo "RUST_${RUST_VERSION}-ZCASH_${ZCASH_VERSION}-ZEBRA_${ZEBRA_VERSION}-DOCKERFILE_${CI_DOCKERFILE_VERSION}"