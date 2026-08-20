#!/usr/bin/env bash
# Deploy the current release-pipeline branch tip to the DISPOSABLE sandbox fork
# and drive a targeted end-to-end test of the backport sentinel:
#
#   - dev    <- current branch tip (has the new relman + workflows + the
#              changeset-check `sync/stable-to-dev` exemption)
#   - stable <- tip + one `chore(release):` commit, so `stable \ dev` is
#              non-empty and the push to stable fires the sentinel.
#
# Why the `chore(release):` prefix: a push to stable also triggers the blessing
# workflow, whose job guard skips `chore(release):`-prefixed commits — so only
# the sentinel runs. It is also the most faithful input: the sentinel exists to
# backport exactly the blessing's release commit.
#
# SENSITIVE — run manually. It toggles branch-protection rulesets and
# force-pushes protected branches. Safe only because the sandbox is a throwaway
# fork; never point REPO at a real repository.
#
# Prereqs: `gh` authenticated with admin on the sandbox; a git remote (default
# `sandbox`) pointing at it; the GitHub App + RELEASE_APP_* secrets already
# configured (they are). Run from the repo root on `feat/release-pipeline`.
set -euo pipefail

REMOTE="${REMOTE:-sandbox}"
REPO="${REPO:-nachog00/zaino-pipeline-sandbox}"
# Ruleset ids for this sandbox (see: gh api repos/$REPO/rulesets).
DEV_RULESET="${DEV_RULESET:-21067396}"
STABLE_RULESET="${STABLE_RULESET:-21067400}"

tip="$(git rev-parse HEAD)"
echo "Deploying ${tip:0:12} to ${REPO} dev + stable ..."

set_enforcement() { # <ruleset-id> <active|disabled>
  gh api -X PUT "repos/${REPO}/rulesets/${1}" -f "enforcement=${2}" \
    --jq '.name + " -> " + .enforcement'
}

# 1. Drop protection so the divergent reset can force-push.
set_enforcement "$DEV_RULESET" disabled
set_enforcement "$STABLE_RULESET" disabled

# 2. Mint the stable tip inline (no branch switch, no working-tree churn): the
#    current tree, parented on the tip, as a `chore(release):` commit.
stable_tip="$(git commit-tree "$(git rev-parse 'HEAD^{tree}')" -p "$tip" \
  -m 'chore(release): cycle-test release commit (sandbox sentinel test)')"

git push -f "$REMOTE" "${tip}:refs/heads/dev"
git push -f "$REMOTE" "${stable_tip}:refs/heads/stable"

# 3. Restore protection. The sentinel writes only `sync/stable-to-dev` (an
#    unprotected branch) and a PR into dev, so it does not need protection off.
set_enforcement "$DEV_RULESET" active
set_enforcement "$STABLE_RULESET" active

cat <<EOF

Deployed. The push to stable fires the backport sentinel.
Watch it:
  gh run list  -R ${REPO} --workflow 'Backport sentinel' --limit 3
  gh run watch -R ${REPO} \$(gh run list -R ${REPO} --workflow 'Backport sentinel' --limit 1 --json databaseId --jq '.[0].databaseId')
Then confirm the PR + that changeset-check was skipped on it:
  gh pr list -R ${REPO} --base dev --head sync/stable-to-dev
EOF
