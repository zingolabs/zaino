#!/usr/bin/env bash
# Run the integration-tests sub-workspace inside the CI container, forwarding
# extra args to `cargo nextest run`.
#
# Sourced as the script.main of the `integration-test` task (extends
# `base-script`); info comes from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "Running integration-tests sub-workspace via container-test"
info "-- manifest: integration-tests/Cargo.toml"

exec makers container-test \
  --manifest-path integration-tests/Cargo.toml "$@"
