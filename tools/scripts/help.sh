#!/usr/bin/env bash
# Print available commands and usage notes.
#
# Sourced as the script.main of the `help` task (extends `base-script`).

set -euo pipefail

echo ""
echo "Zaino CI Image Tasks"
echo "---------------------"
echo ""
echo "Common usage:"
echo "  makers test            # packages/* tests, no live validator (default)"
echo "  makers test live       # both live partitions on the ztest k8s harness"
echo "  makers test all        # everything: packages then live"
echo ""
echo "Available commands:"
echo ""
echo "  test [SET]                 Front door. SET = packages (default) | e2e | \
clientless | live | all | ironwood"
echo "                               packages   packages/* tests (cargo \
nextest run), no live validator"
echo "                               e2e        the e2e live partition (ztest \
/ k8s)"
echo "                               clientless the clientless live partition \
(ztest / k8s)"
echo "                               live       both live partitions (ztest \
/ k8s)"
echo "                               all        packages then live (everything)"
echo "                               ironwood   the ironwood tests of every set"
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
