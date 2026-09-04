#!/usr/bin/env bash
#
# Integration test for the `changeset-check` GitHub Actions workflow.
#
# Proves that the workflow + the `relman` CLI can be exercised end-to-end
# locally with `act` + podman over a self-contained git fixture — no network,
# no cargo in the container, no dependence on the surrounding zaino tree.
#
# What it does:
#   1. Builds a static (musl) `relman` binary once.
#   2. Stands up a minimal fixture repo (one governed target, a bare local
#      `origin`, the real workflow + setup-relman action, and the staged
#      binary) in a temp dir.
#   3. Runs the workflow under `act` for three scenarios and asserts the job's
#      exit code:
#        - covered   + enforce  -> job SUCCEEDS (a covering changeset is present)
#        - uncovered + enforce  -> job FAILS    (enforcement catches the gap)
#        - uncovered + advisory -> job SUCCEEDS (rollout mode: warn, don't block)
#
# Requirements: `act`, rootless `podman`, and the `catthehacker/ubuntu` image.
# See README.md in this directory for the podman/DOCKER_CONFIG setup notes.

set -euo pipefail

# --- Locations -------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

RELMAN_MANIFEST="${REPO_ROOT}/tools/relman/Cargo.toml"
MUSL_TARGET="x86_64-unknown-linux-musl"
RELMAN_REL="tools/relman/target/${MUSL_TARGET}/release/relman"
RELMAN_BIN="${REPO_ROOT}/${RELMAN_REL}"

WORKFLOW_SRC="${REPO_ROOT}/.github/workflows/changeset-check.yml"
ACTION_SRC="${REPO_ROOT}/.github/actions/setup-relman"

PODMAN_SOCK="unix:///run/user/$(id -u)/podman/podman.sock"
ACT_IMAGE="catthehacker/ubuntu:act-latest"

# --- Pretty output ---------------------------------------------------------

log()  { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  PASS\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m  FAIL\033[0m %s\n' "$*"; FAILED=1; }

FAILED=0

# --- Clean, isolated git (never touch the developer's config or GPG) -------

git_c() {
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
  GIT_AUTHOR_NAME=relman-it GIT_AUTHOR_EMAIL=relman-it@example.invalid \
  GIT_COMMITTER_NAME=relman-it GIT_COMMITTER_EMAIL=relman-it@example.invalid \
    git -c commit.gpgsign=false "$@"
}

# --- 1. Build the static relman binary -------------------------------------

build_relman() {
  log "Building a static ${MUSL_TARGET} relman binary"
  rustup target add "${MUSL_TARGET}" >/dev/null 2>&1 || true
  cargo build --release --target "${MUSL_TARGET}" --manifest-path "${RELMAN_MANIFEST}"

  [ -x "${RELMAN_BIN}" ] || { echo "relman binary missing at ${RELMAN_BIN}" >&2; exit 1; }
  # Confirm it is static-ish and actually runs.
  if file "${RELMAN_BIN}" | grep -Eq "static-pie|statically linked"; then
    ok "binary is statically linked ($(file -b "${RELMAN_BIN}" | cut -d, -f1-2))"
  else
    fail "binary does not look static: $(file -b "${RELMAN_BIN}")"
  fi
  "${RELMAN_BIN}" --version >/dev/null && ok "relman --version runs: $("${RELMAN_BIN}" --version)"
}

# --- 2. Build the fixture repo ---------------------------------------------

FIXTURE=""
# Reached via `trap cleanup EXIT`, not a direct call. SC2317/SC2329 are the same
# trap-handler false positive under different linter versions; disable both.
# shellcheck disable=SC2317,SC2329
cleanup() { [ -n "${FIXTURE}" ] && rm -rf "${FIXTURE}"; }
trap cleanup EXIT

build_fixture() {
  FIXTURE="$(mktemp -d)"
  log "Building fixture repo at ${FIXTURE}"

  # Clean DOCKER_CONFIG dir — sidesteps the host's docker-desktop cred helper
  # that otherwise breaks image pulls under act.
  mkdir -p "${FIXTURE}/dockercfg"
  printf '{}\n' > "${FIXTURE}/dockercfg/config.json"

  local work="${FIXTURE}/work"
  mkdir -p "${work}/packages/pkg-a/src" \
           "${work}/.changesets" \
           "${work}/.github/workflows" \
           "${work}/tools/relman/target/${MUSL_TARGET}/release"

  # Minimal governed target manifest — one target, `pkg-a`.
  cat > "${work}/relman.toml" <<'TOML'
[options]
changesets_dir      = ".changesets"
root_manifest       = "Cargo.toml"
workspace_changelog = "CHANGELOG.md"

[[target]]
name = "pkg-a"
path = "packages/pkg-a"
TOML

  # Trivial workspace + member crate (never compiled by the check; present for
  # realism and so relman.toml's root_manifest points at something real).
  cat > "${work}/Cargo.toml" <<'TOML'
[workspace]
resolver = "2"
members = ["packages/pkg-a"]
TOML

  cat > "${work}/packages/pkg-a/Cargo.toml" <<'TOML'
[package]
name = "pkg-a"
version = "0.1.0"
edition = "2021"
publish = false
TOML

  cat > "${work}/packages/pkg-a/src/lib.rs" <<'RS'
pub fn answer() -> u32 {
    42
}
RS

  # Keep the empty changesets dir tracked in the base commit.
  : > "${work}/.changesets/.gitkeep"

  # The real workflow + composite action under test.
  cp "${WORKFLOW_SRC}" "${work}/.github/workflows/changeset-check.yml"
  mkdir -p "${work}/.github/actions/setup-relman"
  cp "${ACTION_SRC}/action.yml" "${work}/.github/actions/setup-relman/action.yml"

  # Stage the prebuilt binary so setup-relman finds it (no in-container build).
  cp "${RELMAN_BIN}" "${work}/${RELMAN_REL}"
  chmod +x "${work}/${RELMAN_REL}"

  # Init the repo and commit the base tree as `dev`.
  git_c -C "${work}" init -q -b dev
  git_c -C "${work}" add relman.toml Cargo.toml packages .changesets/.gitkeep .github
  git_c -C "${work}" commit -qm "fixture: base dev tree"

  # A bare local repo as `origin`, living INSIDE the working tree (untracked,
  # never gitignored) at an absolute path. act's checkout copies the whole tree
  # — including untracked, non-ignored files — into the container at the SAME
  # absolute path, so the in-container `git fetch origin dev` resolves locally.
  git_c -C "${work}" init -q --bare "${work}/origin.git"
  git_c -C "${work}" remote add origin "${work}/origin.git"
  git_c -C "${work}" push -q origin dev

  ok "fixture built (target=pkg-a, bare origin at ${work}/origin.git)"
}

# --- act event payload -----------------------------------------------------
#
# base.ref=dev, not a draft, and head.repo.full_name == repository.full_name
# so any same-repo guard is satisfied.
write_event() {
  cat > "${FIXTURE}/event.json" <<'JSON'
{
  "action": "synchronize",
  "number": 42,
  "pull_request": {
    "number": 42,
    "draft": false,
    "base": { "ref": "dev" },
    "head": { "ref": "feat/change", "repo": { "full_name": "acme/proj" } }
  },
  "repository": { "full_name": "acme/proj" }
}
JSON
}

# --- Run act for one scenario branch ---------------------------------------
#
# Usage: run_act <branch> <enforce:true|"">
# Echoes nothing; returns act's exit code (act exits non-zero iff a job failed).
run_act() {
  local branch="$1" enforce="${2:-}"
  local work="${FIXTURE}/work"

  local -a var_args=()
  [ -n "${enforce}" ] && var_args=(--var "RELMAN_ENFORCE_CHANGESETS=${enforce}")

  local logf="${FIXTURE}/act-${branch//\//-}-${enforce:-advisory}.log"
  (
    cd "${work}"
    git_c checkout -q "${branch}"
    DOCKER_CONFIG="${FIXTURE}/dockercfg" \
      act pull_request \
        -W .github/workflows/changeset-check.yml \
        -e "${FIXTURE}/event.json" \
        --container-daemon-socket "${PODMAN_SOCK}" \
        -P "ubuntu-latest=${ACT_IMAGE}" \
        "${var_args[@]}"
  ) >"${logf}" 2>&1
}

# --- 3. Scenarios ----------------------------------------------------------

make_covered_branch() {
  local work="${FIXTURE}/work"
  git_c -C "${work}" checkout -q dev
  git_c -C "${work}" checkout -q -B covered
  # Touch governed source AND add a covering changeset.
  cat >> "${work}/packages/pkg-a/src/lib.rs" <<'RS'

pub fn added() -> u32 {
    1
}
RS
  cat > "${work}/.changesets/pr.toml" <<'TOML'
[[changes]]
crate = "pkg-a"
kind = "feature"
description = "Add pkg_a::added()."
TOML
  git_c -C "${work}" add packages/pkg-a/src/lib.rs .changesets/pr.toml
  git_c -C "${work}" commit -qm "covered: change pkg-a with a changeset"
}

make_uncovered_branch() {
  local work="${FIXTURE}/work"
  git_c -C "${work}" checkout -q dev
  git_c -C "${work}" checkout -q -B uncovered
  # Touch governed source with NO changeset.
  cat >> "${work}/packages/pkg-a/src/lib.rs" <<'RS'

pub fn orphan() -> u32 {
    2
}
RS
  git_c -C "${work}" add packages/pkg-a/src/lib.rs
  git_c -C "${work}" commit -qm "uncovered: change pkg-a with no changeset"
}

# --- Main ------------------------------------------------------------------

build_relman
build_fixture
write_event
make_covered_branch
make_uncovered_branch

log "Scenario 1/3: covered + enforce -> expect job SUCCEEDS"
if run_act covered true; then
  ok "covered+enforce: act exit 0 (check passed)"
else
  fail "covered+enforce: act exit $? (expected success). Log: ${FIXTURE}/act-covered-true.log"
  tail -30 "${FIXTURE}/act-covered-true.log" || true
fi

log "Scenario 2/3: uncovered + enforce -> expect job FAILS"
if run_act uncovered true; then
  fail "uncovered+enforce: act exit 0 (expected failure). Log: ${FIXTURE}/act-uncovered-true.log"
  tail -30 "${FIXTURE}/act-uncovered-true.log" || true
else
  ok "uncovered+enforce: act exit non-zero (enforcement blocked the PR)"
fi

log "Scenario 3/3: uncovered + advisory -> expect job SUCCEEDS"
if run_act uncovered ""; then
  ok "uncovered+advisory: act exit 0 (missing changeset warned, not blocked)"
else
  fail "uncovered+advisory: act exit $? (expected success). Log: ${FIXTURE}/act-uncovered-advisory.log"
  tail -30 "${FIXTURE}/act-uncovered-advisory.log" || true
fi

echo
if [ "${FAILED}" -eq 0 ]; then
  printf '\033[1;32mALL SCENARIOS GREEN\033[0m\n'
  exit 0
else
  printf '\033[1;31mINTEGRATION TEST FAILED\033[0m\n'
  exit 1
fi
