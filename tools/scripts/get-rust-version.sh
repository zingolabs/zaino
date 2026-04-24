#!/usr/bin/env bash
# Emit the pinned rustc version read from rust-toolchain.toml.
#
# Single source of truth for RUST_VERSION used by the CI image build
# (Makefile.toml build-image, tools/scripts/get-ci-image-tag.sh, and the
# GitHub workflows). Exits non-zero if channel is anything other than a
# concrete numeric version (x.y or x.y.z) — "stable" / "nightly" / dated
# pins would produce a non-reproducible container image tag.
set -euo pipefail

git_root=$(git rev-parse --show-toplevel)
toolchain_file="$git_root/rust-toolchain.toml"

if [[ ! -f "$toolchain_file" ]]; then
  echo "get-rust-version.sh: $toolchain_file not found" >&2
  exit 1
fi

channel=$(
  grep -E '^[[:space:]]*channel[[:space:]]*=' "$toolchain_file" \
    | head -n1 \
    | sed -E 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/'
)

if [[ -z "$channel" ]]; then
  echo "get-rust-version.sh: no [toolchain].channel in $toolchain_file" >&2
  exit 1
fi

if [[ ! "$channel" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "get-rust-version.sh: channel '$channel' is not a concrete numeric version (e.g. 1.95 or 1.95.0)" >&2
  echo "get-rust-version.sh: the CI image requires a pinned rustc; set channel = \"<x.y[.z]>\" in $toolchain_file" >&2
  exit 1
fi

echo "$channel"
