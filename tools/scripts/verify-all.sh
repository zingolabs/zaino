#!/usr/bin/env bash
# Exercise every Makefile.toml task for correctness (idempotent).
#
# Run directly as the `verify-all` task script (does not extend base-script);
# it sources helpers.sh itself for info/warn/err and IMAGE_NAME comes from
# Makefile.toml [env].

set -uo pipefail

# shellcheck source=tools/scripts/helpers.sh
source "./tools/scripts/helpers.sh"

VERIFY_TMPDIR=$(mktemp -d)
PASS=0
FAIL=0
SKIPPED=0

cleanup() {
    info "Cleaning up verification artifacts..."

    # Restore .cargo/config.toml if we saved one
    if [[ -f "$VERIFY_TMPDIR/cargo-config.toml.bak" ]]; then
        mkdir -p .cargo
        cp "$VERIFY_TMPDIR/cargo-config.toml.bak" .cargo/config.toml
    elif [[ -f "$VERIFY_TMPDIR/cargo-config-absent" ]]; then
        rm -f .cargo/config.toml
    fi

    # Restore .cargo/.rocksdb-audit if we saved one
    if [[ -f "$VERIFY_TMPDIR/rocksdb-audit.bak" ]]; then
        cp "$VERIFY_TMPDIR/rocksdb-audit.bak" .cargo/.rocksdb-audit
    elif [[ -f "$VERIFY_TMPDIR/rocksdb-audit-absent" ]]; then
        rm -f .cargo/.rocksdb-audit
    fi

    # Remove test image if we built one
    if [[ -f "$VERIFY_TMPDIR/image-tag" ]]; then
        local tag
        tag=$(cat "$VERIFY_TMPDIR/image-tag")
        info "Removing verification image ${IMAGE_NAME}:verify-${tag}..."
        podman rmi "${IMAGE_NAME}:verify-${tag}" 2>/dev/null || true
    fi

    # Remove temp dir
    rm -rf "$VERIFY_TMPDIR"

    echo ""
    echo "========================================="
    echo "verify-all: ${PASS} passed, ${FAIL} failed, ${SKIPPED} skipped"
    echo "========================================="
    if [[ $FAIL -gt 0 ]]; then
        exit 1
    fi
}
trap cleanup EXIT

run_task() {
    local name="$1"
    shift
    echo ""
    info "--- ${name} ---"
    if "$@" 2>&1; then
        PASS=$((PASS + 1))
        info "PASS: ${name}"
    else
        FAIL=$((FAIL + 1))
        err "FAIL: ${name}"
    fi
}

skip_task() {
    local name="$1"
    local reason="$2"
    echo ""
    warn "--- SKIP: ${name} (${reason}) ---"
    SKIPPED=$((SKIPPED + 1))
}

# === Save state for idempotency ===
if [[ -f .cargo/config.toml ]]; then
    cp .cargo/config.toml "$VERIFY_TMPDIR/cargo-config.toml.bak"
else
    touch "$VERIFY_TMPDIR/cargo-config-absent"
fi
if [[ -f .cargo/.rocksdb-audit ]]; then
    cp .cargo/.rocksdb-audit "$VERIFY_TMPDIR/rocksdb-audit.bak"
else
    touch "$VERIFY_TMPDIR/rocksdb-audit-absent"
fi

# === Tier 1: No container needed ===

run_task "help" makers help
run_task "compute-image-tag" makers compute-image-tag
run_task "get-podman-hash" makers get-podman-hash
run_task "init-podman-volumes" makers init-podman-volumes
run_task "check-matching-zebras" makers check-matching-zebras
run_task "fmt" makers fmt
run_task "clippy" makers clippy
run_task "doc" makers doc

# Rocksdb tasks (save/restore state around them)
if pkg-config --exists rocksdb 2>/dev/null; then
    run_task "check-system-rocksdb" makers check-system-rocksdb
    run_task "use-system-rocksdb" makers use-system-rocksdb
    run_task "audit-system-rocksdb" makers audit-system-rocksdb
    run_task "use-bundled-rocksdb" makers use-bundled-rocksdb
else
    skip_task "rocksdb tasks" "system rocksdb not installed"
fi

# toggle-hooks: run twice to restore original state
run_task "toggle-hooks (on)" makers toggle-hooks
run_task "toggle-hooks (off)" makers toggle-hooks

# === Tier 2: Container build ===

TAG=$(./tools/scripts/get-ci-image-tag.sh)
echo "$TAG" > "$VERIFY_TMPDIR/image-tag"

run_task "build-image" makers build-image
run_task "ensure-image-exists" makers ensure-image-exists

# Tag a verification copy so we can clean up without affecting cached images
podman tag "${IMAGE_NAME}:${TAG}" "${IMAGE_NAME}:verify-${TAG}" \
    2>/dev/null || true

# === Tier 3: Container execution ===
# One test per component: zcashd, zebrad, lightwallet gRPC,
# wallet-to-validator. Tests live in the integration-tests sub-workspace.
ITESTS=(--manifest-path integration-tests/Cargo.toml)

run_task "container-test (zcashd)" makers container-test \
    "${ITESTS[@]}" \
    -E "binary(wallet_to_validator) & test(=zcashd::connect_to_node_get_info)"

run_task "container-test (zebrad)" makers container-test \
    "${ITESTS[@]}" \
    -E "binary(wallet_to_validator) & \
test(=zebrad::state_service::connect_to_node_get_info)"

run_task "container-test (lightwallet)" makers container-test \
    "${ITESTS[@]}" \
    -E "binary(state_service) & test(=zebra::lightwallet_indexer::get_block)"

run_task "container-test (wallet-to-validator)" makers container-test \
    "${ITESTS[@]}" \
    -E "binary(wallet_to_validator) & test(=zcashd::sent_to::transparent)"

if command -v jq &>/dev/null && command -v yq &>/dev/null; then
    run_task "validate-test-targets" makers validate-test-targets
else
    skip_task "validate-test-targets" "jq or yq not installed"
fi

# === Tier 4: Destructive/CI-only (skip) ===

skip_task "push-image" "pushes to registry"
skip_task "update-test-targets" "modifies CI workflow files"
skip_task "validate-makefile-tasks" "redundant with this task"
skip_task "container-test-save-failures" "requires full test run"
skip_task "container-test-retry-failures" "requires prior save-failures"
