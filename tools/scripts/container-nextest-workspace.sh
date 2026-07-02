#!/usr/bin/env bash
# Run one live-test partition inside the CI container, forwarding extra args to
# `cargo nextest run`. The live-test crates are now members of the single root
# workspace, so the partition is selected by package (`-p`) rather than by a
# separate workspace manifest.
#
# Parameterised by the consuming task's [env]:
#   PACKAGE      - the partition crate to test (`clientless` or `e2e`) (required)
#   PACKAGE_DESC - human label for the info line (required)
#
# Sourced as the script.main of the `live-clientless` / `live-e2e` tasks (both
# extend `base-script`); info comes from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "Running ${PACKAGE_DESC} via container-test"
info "-- package: ${PACKAGE}"

exec makers container-test \
  -p "${PACKAGE}" "$@"
