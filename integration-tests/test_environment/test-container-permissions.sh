#!/usr/bin/env bash

# Local container permission tests for zaino image.
# These tests verify the entrypoint correctly handles volume mounts
# and refuses to run as root.
#
# Usage: ./test-container-permissions.sh [image-name]
# Default image: zaino:test-entrypoint

set -uo pipefail

# Enforce rootless podman hardening via repo-wide containers.conf
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../" && pwd)"
export CONTAINERS_CONF_OVERRIDE="${SCRIPT_DIR}/.config/containers.conf"

IMAGE="${1:-zaino:test-entrypoint}"
TEST_DIR="/tmp/zaino-container-tests-$$"
PASSED=0
FAILED=0

cleanup() {
  echo "Cleaning up ${TEST_DIR}..."
  rm -rf "${TEST_DIR}"
}
trap cleanup EXIT

mkdir -p "${TEST_DIR}"

pass() {
  echo "PASS: $1"
  ((PASSED++))
}

fail() {
  echo "FAIL: $1"
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

# Verify the container refuses to run as root
# Override userns so --user 0:0 actually maps to root inside the container
test_refuses_root() {
  podman run --rm --userns=host --user 0:0 "${IMAGE}" --version 2>&1 | grep -q "Refusing to run as root"
}
run_test "refuses to run as root" test_refuses_root

# Basic smoke tests
run_test "help command" \
  podman run --rm "${IMAGE}" --help

run_test "version command" \
  podman run --rm "${IMAGE}" --version

run_test "generate-config command" \
  podman run --rm "${IMAGE}" generate-config

run_test "start --help command" \
  podman run --rm "${IMAGE}" start --help

# Volume mount tests - using /app paths
test_config_mount() {
  local dir="${TEST_DIR}/config"
  mkdir -p "${dir}"
  podman run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  test -f "${dir}/zainod.toml"
}
run_test "config dir mount (/app/config)" test_config_mount

test_data_mount() {
  local dir="${TEST_DIR}/data"
  mkdir -p "${dir}"
  podman run --rm -v "${dir}:/app/data" "${IMAGE}" --version
}
run_test "data dir mount (/app/data)" test_data_mount

# File ownership verification
test_file_ownership() {
  local dir="${TEST_DIR}/ownership-test"
  mkdir -p "${dir}"
  podman run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  local uid
  uid=$(stat -c '%u' "${dir}/zainod.toml" 2>/dev/null || stat -f '%u' "${dir}/zainod.toml")
  test "${uid}" = "1000"
}
run_test "files created with correct UID (1000)" test_file_ownership

# Read-only config mount
test_readonly_config() {
  local dir="${TEST_DIR}/ro-config"
  mkdir -p "${dir}"
  podman run --rm -v "${dir}:/app/config" "${IMAGE}" generate-config
  podman run --rm -v "${dir}:/app/config:ro" "${IMAGE}" --version
}
run_test "read-only config mount" test_readonly_config

# Summary
echo "========================================="
echo "Results: ${PASSED} passed, ${FAILED} failed"
echo "========================================="

if [[ ${FAILED} -gt 0 ]]; then
  exit 1
fi
