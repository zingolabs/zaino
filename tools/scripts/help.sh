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
echo "  makers container-test"
echo ""
echo "If you modify '.env.testing-artifacts', the test command will \
automatically:"
echo "  - Recompute the image tag"
echo "  - Build a new local container image if needed"
echo ""
echo "Available commands:"
echo ""
echo "  container-test             Run integration tests using the local \
image"
echo "  integration-test           Run integration-tests sub-workspace \
(forwards flags to nextest)"
echo "  container-test-save-failures    Run tests, save failures to \
.failed-tests"
echo "  container-test-retry-failures   Rerun only the previously failed \
tests"
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
echo "  check-matching-zebras      Verify Zebra versions match between \
Cargo.toml and .env"
echo "  validate-test-targets      Check if nextest targets match CI \
workflow matrix"
echo "  update-test-targets        Update CI workflow matrix to match \
nextest targets"
echo "  validate-makefile-tasks    Run minimal validation of all maker tasks"
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
echo "  ZCASH_VERSION, ZEBRA_VERSION  Defined by: .env.testing-artifacts"
echo "  RUST_VERSION                  Derived from rust-toolchain.toml"
echo "                                via tools/scripts/get-rust-version.sh"
echo ""
echo "Build Context:"
echo "  integration-tests/test_environment/   Directory containing the \
container build environment"
echo "    ├── Containerfile                 Containerfile for CI/test \
container"
echo "    └── entrypoint.sh                Entrypoint script that sets up \
test binaries"
echo ""
echo "Helpers:"
echo "  - tools/scripts/get-ci-image-tag.sh: computes the version-based \
image tag"
echo "  - tools/scripts/helpers.sh: logging and helper functions"
echo ""
