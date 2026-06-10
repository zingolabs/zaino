#!/usr/bin/env bash
# Run integration tests using the local image.
#
# Runs tests inside the container built from
# integration-tests/test_environment. The container's entrypoint.sh sets up
# test binaries (zcashd, zebrad, zcash-cli) by symlinking
# /home/container_user/artifacts to the expected
# integration-tests/test_binaries/bins location.
#
# Sourced as the script.main of the `container-test` task (extends
# `base-script`); TAG, IMAGE_NAME, TEST_BINARIES_DIR, info, and the cleanup
# trap come from the base-script pre-script
# (tools/scripts/base-script-pre.sh).

set -euo pipefail

info "Running tests using:"
info "-- IMAGE             = ${IMAGE_NAME}"
info "-- TAG               = $TAG"
# info "-- TEST_BINARIES_DIR = ${TEST_BINARIES_DIR}"

# Set container name for cleanup. Suffix with $$ (script PID) so concurrent
# `makers container-test` invocations on the same host don't collide on the
# `zaino-testing` name. The base-script cleanup trap reads CONTAINER_NAME
# from script scope and stops the right container on EXIT/INT/TERM.
CONTAINER_NAME="zaino-testing-$$"

# Run podman in foreground with proper signal handling.
#
# `--pids-limit=-1` removes the default 2048-process cgroup cap. With
# `--profile stable` (test-threads = num-cpus) each integration test
# spawns a full zebrad whose rayon pool itself sizes to num-cpus, so
# peak task count scales ~num_cpus^2. On developer hosts with many
# cores the default cap is breached and `clone()` returns EAGAIN,
# surfacing as "OS can't spawn worker thread" / rayon
# ThreadPoolBuildError panics scattered across unrelated tests.
podman run --rm \
  --init \
  --pids-limit=-1 \
  --name "$CONTAINER_NAME" \
  -v "$PWD":/home/container_user/zaino \
  -v zaino-container-target:/home/container_user/zaino/target:U \
  -v zaino-cargo-git:/usr/local/cargo/git:U \
  -v zaino-cargo-registry:/usr/local/cargo/registry:U \
  -e "TEST_BINARIES_DIR=${TEST_BINARIES_DIR}" \
  -e "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1" \
  -e "ZAINOLOG_FORMAT=${ZAINOLOG_FORMAT:-stream}" \
  -e "RUST_LOG=${RUST_LOG:-}" \
  -w /home/container_user/zaino \
  -u container_user \
  "${IMAGE_NAME}:$TAG" \
  cargo nextest run "$@" &

# Capture the background job PID
PODMAN_PID=$!

# Wait for the podman process
wait "$PODMAN_PID"
