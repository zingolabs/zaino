#!/usr/bin/env bash
set -euo pipefail

# Source shared utility functions
# shellcheck source=tools/scripts/functions.sh
source "$(dirname "${BASH_SOURCE[0]}")/functions.sh"

# Execute the function and output result
get_container_hash


