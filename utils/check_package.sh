#!/bin/bash

# Check if package name is provided
if [ $# -eq 0 ]; then
    echo "Usage: $0 <package-name>"
    echo "Example: $0 zaino-state"
    exit 1
fi

PACKAGE_NAME="$1"

# Run all cargo commands for the specified package
set -e  # Exit on first error

echo "Running checks for package: $PACKAGE_NAME"

cargo check --package "$PACKAGE_NAME" && \
cargo check --all-features --package "$PACKAGE_NAME" && \
cargo check --tests --package "$PACKAGE_NAME" && \
cargo check --tests --all-features --package "$PACKAGE_NAME" && \
cargo fmt --package "$PACKAGE_NAME" && \
cargo clippy --package "$PACKAGE_NAME" && \
cargo nextest run --package "$PACKAGE_NAME"
