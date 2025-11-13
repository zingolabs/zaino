#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# Execute the function and output result
get_docker_hash


