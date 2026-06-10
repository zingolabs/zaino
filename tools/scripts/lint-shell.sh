#!/usr/bin/env bash
# Lint every shell script tracked in the repo with shellcheck.
#
# Run directly (./tools/scripts/lint-shell.sh) or via `makers lint-shell`.
# Configuration (external sources, source paths) lives in .shellcheckrc at the
# repo root.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

mapfile -t scripts < <(git ls-files '*.sh')

if [ "${#scripts[@]}" -eq 0 ]; then
    echo "lint-shell: no shell scripts found."
    exit 0
fi

shellcheck "${scripts[@]}"

echo "lint-shell: ${#scripts[@]} shell script(s) passed shellcheck."
