#!/usr/bin/env bash

# Local Docker permission tests for zaino image.
# These tests verify the entrypoint correctly handles volume mounts
# with various ownership scenarios.
#
# Usage: ./test-docker-permissions.sh [image-name]
# Default image: zaino:test-entrypoint

set -uo pipefail

IMAGE="${1:-zaino:test-entrypoint}"
TEST_DIR="/tmp/zaino-docker-tests-$$"
PASSED=0
FAILED=0

cleanup() {
  echo "Cleaning up ${TEST_DIR}..."
  rm -rf "${TEST_DIR}"
}
trap cleanup EXIT

mkdir -p "${TEST_DIR}"

pass() {
  echo "✅ $1"
  ((PASSED++))
}

fail() {
  echo "❌ $1"
  ((FAILED++))
}

run_test() {
  local name="$1"
  shift
  echo "--- Testing: ${name} ---"
  if "$@"; then
    pass "${name}"
  else
    fail "${name}"
  fi
  echo
}

# Basic smoke tests
run_test "help command" \
  docker run --rm "${IMAGE}" --help

run_test "version command" \
  docker run --rm "${IMAGE}" --version

run_test "generate-config command" \
  docker run --rm "${IMAGE}" generate-config

run_test "start --help command" \
  docker run --rm "${IMAGE}" start --help

# Volume mount tests - using /app paths
test_config_mount() {
  local dir="${TEST_DIR}/config"
  mkdir -p "${dir}"
  docker run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  test -f "${dir}/zainod.toml"
}
run_test "config dir mount (/app/config)" test_config_mount

test_data_mount() {
  local dir="${TEST_DIR}/data"
  mkdir -p "${dir}"
  docker run --rm -v "${dir}:/app/data" "${IMAGE}" --version
}
run_test "data dir mount (/app/data)" test_data_mount

# File ownership verification
test_file_ownership() {
  local dir="${TEST_DIR}/ownership-test"
  mkdir -p "${dir}"
  docker run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  # File should be owned by UID 1000 (container_user)
  local uid
  uid=$(stat -c '%u' "${dir}/zainod.toml" 2>/dev/null || stat -f '%u' "${dir}/zainod.toml")
  test "${uid}" = "1000"
}
run_test "files created with correct UID (1000)" test_file_ownership

# Root-owned directory tests (requires sudo)
if command -v sudo &>/dev/null && sudo -n true 2>/dev/null; then
  test_root_owned_mount() {
    local dir="${TEST_DIR}/root-owned"
    sudo mkdir -p "${dir}"
    sudo chown root:root "${dir}"
    docker run --rm -v "${dir}:/app/data" "${IMAGE}" --version
    # Entrypoint should have chowned it
    local uid
    uid=$(stat -c '%u' "${dir}" 2>/dev/null || stat -f '%u' "${dir}")
    test "${uid}" = "1000"
  }
  run_test "root-owned dir gets chowned" test_root_owned_mount

  test_root_owned_config_write() {
    local dir="${TEST_DIR}/root-config"
    sudo mkdir -p "${dir}"
    sudo chown root:root "${dir}"
    docker run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
    test -f "${dir}/zainod.toml"
  }
  run_test "write to root-owned config dir" test_root_owned_config_write
else
  echo "⚠️  Skipping root-owned tests (sudo not available or requires password)"
fi

# Read-only config mount
test_readonly_config() {
  local dir="${TEST_DIR}/ro-config"
  mkdir -p "${dir}"
  # First generate a config
  docker run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  # Then mount it read-only and verify we can still run
  docker run --rm -v "${dir}:/app/config:ro" "${IMAGE}" --version
}
run_test "read-only config mount" test_readonly_config

# Summary
echo "========================================="
echo "Results: ${PASSED} passed, ${FAILED} failed"
echo "========================================="

if [[ ${FAILED} -gt 0 ]]; then
  exit 1
fi
