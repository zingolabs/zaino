#!/bin/bash

# Check if crate name is provided
if [ $# -eq 0 ]; then
    echo "Usage: $0 <crate-name>"
    echo "Example: $0 zaino-state"
    exit 1
fi

CRATE_NAME="$1"

# Run all cargo commands for the specified crate
set -e  # Exit on first error

echo "Running checks for crate: $CRATE_NAME"

cargo check -p "$CRATE_NAME" && \
cargo check --all-features -p "$CRATE_NAME" && \
cargo check --tests -p "$CRATE_NAME" && \
cargo check --tests --all-features -p "$CRATE_NAME" && \
cargo fmt -p "$CRATE_NAME" && \
cargo clippy -p "$CRATE_NAME" && \
cargo nextest run -p "$CRATE_NAME"