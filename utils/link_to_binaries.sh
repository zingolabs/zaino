#!/bin/bash
set -e

# Usage: link_to_binaries.sh [REPO_ROOT] [ZCASHD_PATH] [ZEBRAD_PATH] [ZCASH_CLI_PATH]
#
# Arguments:
#   REPO_ROOT     - Repository root directory (default: /home/container_user/zaino)
#   ZCASHD_PATH   - Path to zcashd binary (default: /home/container_user/artifacts/zcashd)
#   ZEBRAD_PATH   - Path to zebrad binary (default: /home/container_user/artifacts/zebrad)
#   ZCASH_CLI_PATH - Path to zcash-cli binary (default: /home/container_user/artifacts/zcash-cli)

# Use provided arguments or defaults
REPO_ROOT="${1:-/home/container_user/zaino}"
ZCASHD_PATH="${2:-/home/container_user/artifacts/zcashd}"
ZEBRAD_PATH="${3:-/home/container_user/artifacts/zebrad}"
ZCASH_CLI_PATH="${4:-/home/container_user/artifacts/zcash-cli}"

# Try to create necessary directories for cargo if they don't exist
# This helps with permission issues in CI environments
mkdir -p "${HOME}/.cargo" "${HOME}/target" 2>/dev/null || true

# If running in GitHub Actions, try to ensure workspace is usable
if [ -n "${GITHUB_WORKSPACE}" ] && [ -d "${GITHUB_WORKSPACE}" ]; then
    # Try to create .cargo directories if possible
    mkdir -p "${GITHUB_WORKSPACE}/.cargo" 2>/dev/null || true
    mkdir -p "${GITHUB_WORKSPACE}/target" 2>/dev/null || true
fi

# Check if test_binaries/bins directory exists and create symlinks if binaries are missing
BINS_DIR="${REPO_ROOT}/test_binaries/bins"

if [ -d "$BINS_DIR" ]; then
    echo "Checking for test binaries in $BINS_DIR..."
    
    # Check and create symlink for zcashd
    if [ ! -f "$BINS_DIR/zcashd" ]; then
        echo "zcashd not found in $BINS_DIR, creating symlink..."
        ln -s "$ZCASHD_PATH" "$BINS_DIR/zcashd"
    fi
    
    # Check and create symlink for zebrad
    if [ ! -f "$BINS_DIR/zebrad" ]; then
        echo "zebrad not found in $BINS_DIR, creating symlink..."
        ln -s "$ZEBRAD_PATH" "$BINS_DIR/zebrad"
    fi
    
    # Check and create symlink for zcash-cli
    if [ ! -f "$BINS_DIR/zcash-cli" ]; then
        echo "zcash-cli not found in $BINS_DIR, creating symlink..."
        ln -s "$ZCASH_CLI_PATH" "$BINS_DIR/zcash-cli"
    fi
    
    echo "Binary setup complete. Contents of $BINS_DIR:"
    ls -la "$BINS_DIR"
else
    echo "Info: $BINS_DIR will be created when binaries are first accessed"
fi

# Execute any command passed to the entrypoint
if [ $# -gt 0 ]; then
    exec "$@"
fi
