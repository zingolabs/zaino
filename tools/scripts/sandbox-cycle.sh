#!/usr/bin/env bash
# Step-by-step driver for exercising the release pipeline on the DISPOSABLE
# sandbox fork, one stage per invocation, so a full cycle can be watched live.
#
#   sandbox-cycle.sh align                         # rc + release-ready := dev (clean start)
#   sandbox-cycle.sh feature <slug> <crate> <kind> "<desc>"   # open a changeset PR into dev
#   sandbox-cycle.sh merge <pr#>                   # merge a dev PR
#   sandbox-cycle.sh rc                            # dispatch rc-gate (the "nightly")
#   sandbox-cycle.sh deploy-pass                   # simulate deployment-gate pass: release-ready := rc
#   sandbox-cycle.sh bless                         # merge the open release PR into stable
#   sandbox-cycle.sh status                        # branch tips / tags / open PRs
#
# The deployment gate itself is Argo/devops-side and unbuilt, so `deploy-pass`
# stands in for it by fast-forwarding release-ready to rc.
#
# SENSITIVE — run manually. Toggles branch-protection rulesets, force-pushes /
# merges protected branches. Safe only because the sandbox is a throwaway fork.
set -euo pipefail

REMOTE="${REMOTE:-sandbox}"
REPO="${REPO:-nachog00/zaino-pipeline-sandbox}"
DEV_RULESET="${DEV_RULESET:-21067396}"
RC_RULESET="${RC_RULESET:-21067398}"
RR_RULESET="${RR_RULESET:-21067399}"
STABLE_RULESET="${STABLE_RULESET:-21067400}"

set_enforcement() { # <ruleset-id> <active|disabled>
  gh api -X PUT "repos/${REPO}/rulesets/${1}" -f "enforcement=${2}" \
    --jq '.name + " -> " + .enforcement'
}

cmd_reset() { # [base-ref]  — pristine slate: all 4 branches := base, no changesets/tags
  local base; base="$(git rev-parse "${1:-HEAD}")"
  echo "Resetting all branches to ${base:0:12}; closing spin PRs; clearing tags/releases..."
  local pr
  for pr in $(gh pr list -R "$REPO" --state open --json number,headRefName \
      --jq '.[] | select(.headRefName|startswith("spin/")) | .number'); do
    gh pr close "$pr" -R "$REPO" --delete-branch || true
  done
  local r b
  for r in "$DEV_RULESET" "$RC_RULESET" "$RR_RULESET" "$STABLE_RULESET"; do set_enforcement "$r" disabled; done
  for b in dev rc release-ready stable; do git push -f "$REMOTE" "${base}:refs/heads/${b}"; done
  local tags t
  mapfile -t tags < <(git ls-remote --tags "$REMOTE" \
    | sed -n 's#.*refs/tags/\([^^]*\)$#\1#p' | grep -E '^cycle-|-v[0-9]' | sort -u)
  for t in "${tags[@]}"; do echo "  del tag $t"; git push --quiet "$REMOTE" ":refs/tags/$t" || true; done
  local id
  for id in $(gh api "repos/${REPO}/releases" --jq '.[].id'); do
    echo "  del release $id"; gh api -X DELETE "repos/${REPO}/releases/${id}" || true
  done
  for r in "$DEV_RULESET" "$RC_RULESET" "$RR_RULESET" "$STABLE_RULESET"; do set_enforcement "$r" active; done
  echo "Pristine. All branches at ${base:0:12}; no cycle tags; next bless = cycle-1."
}

cmd_align() {
  git fetch -q "$REMOTE"
  local dev; dev="$(git rev-parse "${REMOTE}/dev")"
  echo "Aligning rc + release-ready to dev (${dev:0:12})"
  set_enforcement "$RC_RULESET" disabled
  set_enforcement "$RR_RULESET" disabled
  git push -f "$REMOTE" "${dev}:refs/heads/rc"
  git push -f "$REMOTE" "${dev}:refs/heads/release-ready"
  set_enforcement "$RC_RULESET" active
  set_enforcement "$RR_RULESET" active
  echo "Aligned. dev == rc == release-ready."
}

cmd_feature() { # <slug> <crate> <kind> <desc>
  local slug="$1" crate="$2" kind="$3" desc="$4"
  git fetch -q "$REMOTE" dev
  local base uid content blob tmpidx tree commit
  base="$(git rev-parse "${REMOTE}/dev")"
  uid="$(cat /proc/sys/kernel/random/uuid)"   # canonical lowercase UUID = a valid relman Uid
  content="$(printf 'id = "%s"\n\n[[changes]]\ncrate = "%s"\nkind = "%s"\ndescription = "%s"\n' \
    "$uid" "$crate" "$kind" "$desc")"
  tmpidx="$(mktemp)"
  GIT_INDEX_FILE="$tmpidx" git read-tree "$base"
  blob="$(printf '%s' "$content" | git hash-object -w --stdin)"
  GIT_INDEX_FILE="$tmpidx" git update-index --add --cacheinfo "100644,${blob},.changesets/${slug}.toml"
  tree="$(GIT_INDEX_FILE="$tmpidx" git write-tree)"
  rm -f "$tmpidx"
  commit="$(git commit-tree "$tree" -p "$base" \
    -m "feat(${crate}): ${desc} [sandbox spin]")"
  git push "$REMOTE" "${commit}:refs/heads/spin/${slug}"
  gh pr create -R "$REPO" --base dev --head "spin/${slug}" \
    --title "feat(${crate}): ${desc}" \
    --body "Sandbox spin: ${kind} change to ${crate}. Changeset id ${uid} (the rename bot will rename \`${slug}.toml\` -> \`pr-<N>.toml\`)."
}

cmd_merge() { # <pr#>
  gh pr merge "$1" -R "$REPO" --merge --delete-branch
  echo "Merged PR #$1 into dev."
}

cmd_rc() {
  gh workflow run rc-gate.yml -R "$REPO" --ref dev
  echo "Dispatched rc-gate. Watch: gh run list -R ${REPO} --workflow 'RC gate' --limit 3"
}

cmd_deploy_pass() {
  git fetch -q "$REMOTE"
  local rc; rc="$(git rev-parse "${REMOTE}/rc")"
  echo "Deployment gate is unbuilt (Argo/devops) — simulating PASS: release-ready := rc (${rc:0:12})"
  set_enforcement "$RR_RULESET" disabled
  git push "$REMOTE" "${rc}:refs/heads/release-ready"
  set_enforcement "$RR_RULESET" active
  echo "release-ready advanced. release-pr-body will (re)create the standing release PR."
}

cmd_bless() {
  local pr
  pr="$(gh pr list -R "$REPO" --base stable --head release-ready --state open \
    --json number --jq '.[0].number // empty')"
  [ -n "$pr" ] || { echo "No open release PR (release-ready -> stable). Run deploy-pass first?"; return 1; }
  echo "Merging release PR #${pr} into stable — blessing + backport sentinel will fire."
  set_enforcement "$STABLE_RULESET" disabled
  gh pr merge "$pr" -R "$REPO" --merge
  set_enforcement "$STABLE_RULESET" active
}

cmd_status() {
  git fetch -q "$REMOTE" 2>/dev/null || true
  echo "branches:"
  local b
  for b in dev rc release-ready stable; do
    printf '  %-14s %s\n' "$b" "$(git rev-parse --short "${REMOTE}/${b}" 2>/dev/null || echo '-')"
  done
  echo "tags:"
  git ls-remote --tags "$REMOTE" \
    | sed -n 's#.*refs/tags/\([^^]*\)$#\1#p' | grep -E '^cycle|-v[0-9]' | sort -u | sed 's/^/  /' || true
  echo "open PRs:"
  gh pr list -R "$REPO" --state open \
    --json number,headRefName,baseRefName --jq '.[] | "  #\(.number) \(.headRefName)->\(.baseRefName)"'
}

case "${1:-}" in
  reset)       cmd_reset "${2:-}" ;;
  align)       cmd_align ;;
  feature)     shift; cmd_feature "$@" ;;
  merge)       cmd_merge "${2:?pr number}" ;;
  rc)          cmd_rc ;;
  deploy-pass) cmd_deploy_pass ;;
  bless)       cmd_bless ;;
  status)      cmd_status ;;
  *) echo "usage: $0 {reset [base]|align|feature <slug> <crate> <kind> <desc>|merge <pr#>|rc|deploy-pass|bless|status}" >&2; exit 2 ;;
esac
