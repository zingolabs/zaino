#!/usr/bin/env bash
# Run one test workspace inside the CI container, forwarding extra args to
# `cargo nextest run`. The workspace is selected by its manifest so this works
# for both the integration-tests workspace and the standalone wallet-tests
# workspace (the zingolib wallet stack lives there, kept out of the
# zingolib-free integration-tests workspace).
#
# Parameterised by the consuming task's [env]:
#   WORKSPACE_MANIFEST - path to the workspace Cargo.toml (required)
#   WORKSPACE_DESC     - human label for the info line (required)
#
# Sourced as the script.main of the `walletless-integration-test` / `wallet-integration-test`
# tasks (both extend `base-script`); info comes from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "Running ${WORKSPACE_DESC} via container-test"
info "-- manifest: ${WORKSPACE_MANIFEST}"

exec makers container-test \
  --manifest-path "${WORKSPACE_MANIFEST}" "$@"
