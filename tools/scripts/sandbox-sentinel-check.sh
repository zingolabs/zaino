#!/usr/bin/env bash
# Deploy the current release-pipeline branch tip to the DISPOSABLE sandbox fork
# and drive a targeted end-to-end test of the backport sentinel:
#
#   - dev    <- current branch tip (has the new relman + workflows + the
#              changeset-check `sync/stable-to-dev` exemption)
#   - stable <- tip + one empty commit, so `stable \ dev` is non-empty and the
#              push to stable fires the sentinel.
#
# The push to stable also triggers the blessing workflow, but its provenance
# guard releases only the merge of the release PR (release-ready -> stable);
# a direct push like this one is skipped with a warning, so only the sentinel
# does any work.
#
# SENSITIVE — run manually. It toggles branch-protection rulesets and
# force-pushes protected branches. Safe only because the sandbox is a throwaway
# fork; never point REPO at a real repository.
#
# Prereqs: `gh` authenticated with admin on the sandbox; a git remote (default
# `sandbox`) pointing at it; the GitHub App + RELEASE_APP_* secrets already
# configured (they are). Run from the repo root on `feat/release-pipeline`.
set -euo pipefail

# shellcheck source=tools/scripts/sandbox-common.sh
source "$(dirname "${BASH_SOURCE[0]}")/sandbox-common.sh"

tip="$(git rev-parse HEAD)"
echo "Deploying ${tip:0:12} to ${REPO} dev + stable ..."

# 1. Drop protection so the divergent reset can force-push.
set_enforcement "$DEV_RULESET" disabled
set_enforcement "$STABLE_RULESET" disabled

# 2. Mint the stable tip inline (no branch switch, no working-tree churn): the
#    current tree, parented on the tip, as a commit that stands in for a
#    release commit the sentinel must carry back.
stable_tip="$(git commit-tree "$(git rev-parse 'HEAD^{tree}')" -p "$tip" \
  -m 'test: stand-in release commit (sandbox sentinel test)')"

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
