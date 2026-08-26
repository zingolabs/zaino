#!/usr/bin/env bash
# Stand up (or re-stand-up) a disposable FORK as a release-pipeline sandbox.
#
# The pipeline workflows push protected branches, open PRs, tag, and (in
# dry-run) publish. You cannot safely rehearse that on zingolabs/zaino, but a
# fork you own can be butchered freely. This script does the scriptable setup;
# the two click-ops it cannot do (fork, install the GitHub App) are printed at
# the end.
#
# Usage:
#   tools/scripts/pipeline-fork-sandbox.sh <owner/fork-repo> [--apply]
#
# Without --apply it PRINTS what it would do (dry run). With --apply it mutates
# the fork. Requires: gh (authenticated), and the fork already created from
# zingolabs/zaino (so `dev` and `stable` exist).
#
# It is idempotent: re-running is safe.
set -euo pipefail

REPO="${1:-}"
APPLY="${2:-}"
if [ -z "$REPO" ]; then
  echo "usage: $0 <owner/fork-repo> [--apply]" >&2
  exit 2
fi
if [ "$REPO" = "zingolabs/zaino" ]; then
  echo "refusing to run against the upstream repo. Use a fork." >&2
  exit 2
fi

run() {
  if [ "$APPLY" = "--apply" ]; then
    echo "+ $*"; "$@"
  else
    echo "[dry-run] $*"
  fi
}

echo "== Fork pipeline sandbox: $REPO =="
echo "   mode: ${APPLY:-dry-run (pass --apply to mutate)}"
echo

# --- 1. Branches: rc, release-ready (from dev). dev/stable exist from the fork.
default_sha="$(gh api "repos/$REPO/git/ref/heads/dev" --jq .object.sha 2>/dev/null || true)"
if [ -z "$default_sha" ]; then
  echo "!! could not read refs/heads/dev on $REPO — is the fork created and does it have a dev branch?" >&2
  exit 1
fi
for br in rc release-ready; do
  if gh api "repos/$REPO/git/ref/heads/$br" >/dev/null 2>&1; then
    echo "   branch $br already exists — leaving as-is"
  else
    run gh api "repos/$REPO/git/refs" -X POST \
      -f ref="refs/heads/$br" -f sha="$default_sha"
  fi
done
echo

# --- 2. Repo variables (sandbox defaults: everything ON, publish DRY).
declare -A VARS=(
  [RELMAN_PIPELINE_ACTIVE]=true      # master switch: activate the gated workflows
  [RELMAN_ENFORCE_CHANGESETS]=true   # make the dev-gate changeset check blocking
  [RELMAN_PUBLISH_DRY_RUN]=true      # blessing publishes with --dry-run (no upload)
)
for k in "${!VARS[@]}"; do
  run gh variable set "$k" --repo "$REPO" --body "${VARS[$k]}"
done
echo

# --- 3. Branch protection rulesets (PR required; no force-push/delete).
# Approvals: 1 into dev, 2 into stable (per the ADR); rc/release-ready are
# CI-advanced so they require PRs but 0 human approvals. The GitHub App must be
# added to each ruleset's bypass list to push protected branches / tags — set
# APP_INTEGRATION_ID (the App's *installation* integration id) to include it,
# else add it in the UI after installing the App.
bypass='[]'
if [ -n "${APP_INTEGRATION_ID:-}" ]; then
  bypass="[{\"actor_id\": ${APP_INTEGRATION_ID}, \"actor_type\": \"Integration\", \"bypass_mode\": \"always\"}]"
fi
ruleset() { # <branch> <approvals>
  local br="$1" approvals="$2"
  local body
  body="$(cat <<JSON
{
  "name": "protect-$br",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/heads/$br"], "exclude": [] } },
  "bypass_actors": $bypass,
  "rules": [
    { "type": "pull_request",
      "parameters": { "required_approving_review_count": $approvals,
        "dismiss_stale_reviews_on_push": false, "require_code_owner_review": false,
        "require_last_push_approval": false, "required_review_thread_resolution": false } },
    { "type": "non_fast_forward" },
    { "type": "deletion" }
  ]
}
JSON
)"
  if [ "$APPLY" = "--apply" ]; then
    echo "+ create ruleset protect-$br (approvals=$approvals)"
    echo "$body" | gh api "repos/$REPO/rulesets" -X POST --input - >/dev/null \
      && echo "  created" || echo "  (may already exist — edit/delete in Settings ▸ Rules)"
  else
    echo "[dry-run] create ruleset protect-$br (approvals=$approvals)"
  fi
}
ruleset dev 1
ruleset rc 0
ruleset release-ready 0
ruleset stable 2
echo

# --- 4. Click-ops this script cannot do (printed, not automated).
cat <<'NEXT'
== Manual steps (cannot be scripted) ==
1. Fork zingolabs/zaino to your account/org if you haven't (this script assumes it exists).
2. Create a GitHub App (Settings ▸ Developer settings ▸ GitHub Apps):
   - Permissions: Repository -> Contents: Read & write, Pull requests: Read & write.
   - Install it on the fork.
   - Add it to the rc/release-ready/stable rulesets' bypass list (or re-run this
     script with APP_INTEGRATION_ID=<id> --apply).
   - Store its App ID + a private key as repo secrets RELEASE_APP_ID / RELEASE_APP_PRIVATE_KEY.
3. Add a repo secret CARGO_REGISTRY_TOKEN (any value works while RELMAN_PUBLISH_DRY_RUN=true).
4. Drive the flow: open a PR with a changeset -> merge to dev -> run the "RC gate"
   workflow (dispatch) -> merge the release PR to stable -> watch "Blessing" run
   bump/changelog/tags/publish(--dry-run). Butcher and repeat freely.
NEXT
