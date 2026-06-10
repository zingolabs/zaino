#!/usr/bin/env bash
set -euo pipefail

cargo check --all-features \
  && cargo check --tests --all-features \
  && cargo fmt \
  && cargo clippy \
  && ./tools/scripts/trailing-whitespace.sh reject
