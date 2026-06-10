#!/usr/bin/env bash
# Check that zebra versions in .env.testing-artifacts match Cargo.toml.
#
# Sourced as the script.main of the `check-matching-zebras` task (extends
# `base-script`); ZEBRA_VERSION, info, and err come from the base-script
# pre-script (tools/scripts/base-script-pre.sh) and Makefile.toml [env].

set -euo pipefail

# Verify ZEBRA_VERSION from .env.testing-artifacts is consistent with
# Cargo.toml. Zebra deps may be crates.io versions ("6.0.1") or git revs
# (rev="abc123"). The .env ZEBRA_VERSION is the Docker image tag (e.g.
# "4.3.0").

# Check for git rev-based deps first
cargo_toml=$(sed 's/#.*//' Cargo.toml | tr -d '[:space:]')
zebra_revs=$(
  echo "$cargo_toml" \
    | grep -o 'zebra-[a-z]*={[^}]*rev="[^"]*"' \
    | grep -o 'rev="[^"]*"' \
    | cut -d'"' -f2 \
    | sort -u || true
)

if [[ -n "$zebra_revs" ]]; then
  rev_count=$(echo "$zebra_revs" | wc -l)
  if [[ "$rev_count" -ne 1 ]]; then
    err "❌ Multiple Zebra revs detected in Cargo.toml:"
    echo "$zebra_revs"
    exit 1
  fi
  # Accept short SHA match
  if [[ "$zebra_revs" != "$ZEBRA_VERSION" \
        && "${zebra_revs:0:${#ZEBRA_VERSION}}" != "$ZEBRA_VERSION" ]]; then
    err "❌ Mismatch: Cargo.toml has rev $zebra_revs, but .env has \
$ZEBRA_VERSION"
    exit 1
  fi
else
  # Crates.io version deps -- just verify the Docker image tag is set
  if [[ -z "$ZEBRA_VERSION" ]]; then
    err "❌ ZEBRA_VERSION is not set in .env.testing-artifacts"
    exit 1
  fi
  info "Zebra deps are crates.io versions; Docker image tag is \
${ZEBRA_VERSION}"
fi

info "✅ Zebra version check passed (.env ZEBRA_VERSION=${ZEBRA_VERSION})"
