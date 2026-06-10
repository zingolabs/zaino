#!/usr/bin/env bash
#
# Pre-script for the `base-script` cargo-make task (Makefile.toml).
#
# This file is *sourced* (not executed) as the `script.pre` of `base-script`.
# cargo-make concatenates a task's pre/main/post into a single shell program,
# so everything defined here -- the `TAG` variable, the `podman_cleanup`
# function, and the EXIT/INT/TERM trap -- stays in scope for the task's
# `script.main`. Do not add `set -euo pipefail` here: the original inline
# pre-script did not set it, and each consuming `script.main` sets its own
# shell options.

# shellcheck source=tools/scripts/helpers.sh
source "./tools/scripts/helpers.sh"
# TAG is consumed by each task's script.main after this file is sourced into
# the same shell, so it is not unused here (SC2034 cannot see across sourcing).
# shellcheck disable=SC2034
TAG=$(./tools/scripts/get-ci-image-tag.sh)

# Generic cleanup function for containers
podman_cleanup() {
    # Capture the triggering exit code before anything else clobbers $?.
    local exit_code=$?

    # Prevent running cleanup twice
    if [ "${CLEANUP_RAN:-0}" -eq 1 ]; then
        return
    fi
    CLEANUP_RAN=1

    # Check if we're cleaning up due to interruption
    if [ "$exit_code" -eq 130 ] || [ "$exit_code" -eq 143 ]; then
        echo ""
        warn "Task '${CARGO_MAKE_CURRENT_TASK_NAME}' interrupted! \
Cleaning up..."
    fi

    # Kill all child processes
    local pids
    mapfile -t pids < <(jobs -pr)
    if [ "${#pids[@]}" -gt 0 ]; then
        kill "${pids[@]}" 2>/dev/null || true
    fi

    # Stop any containers started by this script
    if [ -n "${CONTAINER_ID:-}" ]; then
        info "Stopping container..."
        podman stop "$CONTAINER_ID" >/dev/null 2>&1 || true
    fi

    # Also stop by name if CONTAINER_NAME is set
    if [ -n "${CONTAINER_NAME:-}" ] && [ -z "${CONTAINER_ID:-}" ]; then
        info "Stopping container ${CONTAINER_NAME}..."
        podman stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
    fi
}

# Set up cleanup trap
trap podman_cleanup EXIT INT TERM
