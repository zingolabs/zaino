#!/usr/bin/env bash
# Print available commands and usage notes.
#
# Sourced as the script.main of the `help` task (extends `base-script`).

set -euo pipefail

echo ""
echo "Zaino CI Image Tasks"
echo "---------------------"
echo ""
echo "  cargo nextest run                             # packages/*, no validator"
echo "  cd live-tests && ztest run -p clientless -p e2e  # live, on the k8s harness"
echo "  See docs/testing.md for ztest CLI and cluster setup."
echo ""
echo "Available commands:"
echo ""
echo "  build-image                Build the container image with current \
artifact versions"
echo "  push-image                 Push the image (used in CI, can be used \
manually)"
echo "  compute-image-tag          Compute the tag for the container image \
based on versions"
echo "  get-podman-hash            Get CONTAINER_DIR_HASH value (hash for \
the image defining files)"
echo "  ensure-image-exists        Check if the required image exists \
locally, build if not"
echo "  pull-ci-image              Pull the CI image from the registry"
echo "  bench [SUB]                Benchmark a running zainod. SUB = sync | \
concurrent | serve"
echo "                               sync       time an initial sync via \
zainod's /metrics endpoint"
echo "                               concurrent load-test with N concurrent \
block-range clients"
echo "                               serve      single-stream block serve \
rate + chain check"
echo "                             See docs/perf.md for results and the \
measured config."
echo "  verify-all                 Exercise every task for correctness \
(idempotent)"
echo "  hello-rust                 Test rust-script functionality"
echo ""
echo "Lint commands:"
echo "  lint                       Run all lints (fmt, clippy, doc). Use as \
a pre-commit hook."
echo "  fmt                        Check formatting (cargo fmt --all -- \
--check)"
echo "  clippy                     Run Clippy with -D warnings (--all-targets \
--all-features)"
echo "  doc                        Build docs (no deps, all features, \
document private items) with RUSTDOCFLAGS='-D warnings'"
echo "  toggle-hooks               Toggle the git config for core.hooksPath \
to use .githooks/"
echo ""
echo "Build speed:"
echo "  use-system-rocksdb         Link against system RocksDB (skips slow \
C++ build)"
echo "  use-bundled-rocksdb        Revert to building RocksDB from source"
echo "  check-system-rocksdb       Check system RocksDB compatibility"
echo "  audit-system-rocksdb       Re-audit if Cargo.lock or system version \
changed"
echo "  set-worktree-parent-tools  Copy .cargo/config.toml to common \
worktree parent"
echo ""
echo "Environment:"
echo "  RUST_VERSION                  Derived from rust-toolchain.toml"
echo "                                via the workbench get-rust-version bin"
echo ""
echo "Build Context:"
echo "  live-tests/test_environment/   Directory containing the \
CI build-environment image"
echo "    └── Containerfile                 Rust toolchain + protoc + RocksDB \
+ cargo-nextest (no validators)"
echo ""
echo "Helpers:"
echo "  - tools/scripts/get-ci-image-tag.sh: computes the version-based \
image tag"
echo "  - tools/scripts/helpers.sh: logging and helper functions"
echo ""
