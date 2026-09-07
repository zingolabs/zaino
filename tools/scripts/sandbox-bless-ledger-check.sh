#!/usr/bin/env bash
# Drive a targeted end-to-end check of the consume→ledger→bless→sentinel path on
# the DISPOSABLE sandbox fork, without staging a full dev→rc→release-ready cycle.
#
# It force-deploys the current branch tip, puts one changeset (with a UID) for a
# governed crate on `release-ready`, and merges the release PR
# (release-ready -> stable), which is the provenance blessing requires. The
# merge into `stable`:
#   1. fires BLESSING → relman restores the ledger from the last cycle tag
#      (none here → empty), derives the bump, then `consume` stamps the changeset
#      `consumed_in` AND appends its UID to `.release/consumed-ledger.toml`; the
#      release commit + `cycle-1` / `<crate>-X.Y.Z` tags + GitHub Release land;
#   2. fires the BACKPORT SENTINEL (stable now ahead of dev) → it opens the
#      `sync/stable-to-dev` PR with auto-merge; dev has no required checks or
#      approvals, so it merges, carrying the ledger + marks back to `dev`.
#
# Verify afterward (read-only) that stable's ledger lists the UID, the changeset
# is marked consumed, the tags/Release exist, and the sync PR auto-merged.
#
# SENSITIVE — run manually. Toggles branch-protection rulesets, force-pushes
# protected branches, deletes tags/releases, and flips repo settings. Safe only
# because the sandbox is a throwaway fork; never point REPO at a real repository.
set -euo pipefail

# shellcheck source=tools/scripts/sandbox-common.sh
source "$(dirname "${BASH_SOURCE[0]}")/sandbox-common.sh"
# A governed crate (must be a target in relman.toml) for the test changeset.
CRATE="${CRATE:-zaino-state}"
# A fixed, canonical UUID so the verification can grep for it deterministically.
LEDGER_UID="${LEDGER_UID:-018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f}"

tip="$(git rev-parse HEAD)"
echo "Driving ledger+bless+sentinel check on ${REPO} from ${tip:0:12} ..."

# 0. Enable auto-merge so the sentinel's sync PR can self-merge.
gh api -X PATCH "repos/${REPO}" -F allow_auto_merge=true --jq '"allow_auto_merge=" + (.allow_auto_merge|tostring)'

# 1. Drop protection for the divergent reset.
set_enforcement "$DEV_RULESET" disabled
set_enforcement "$RR_RULESET" disabled
set_enforcement "$STABLE_RULESET" disabled

# 2. Clean slate: delete all cycle/prerelease/crate tags + any GitHub Release, so
#    blessing releases a fresh `cycle-1` and the ledger restore finds no prior.
delete_pipeline_tags_and_releases

# 3. Build a `release-ready` tip = current tree + one governed-crate changeset
#    carrying a UID. Uses a throwaway index so the working tree is never touched.
changeset="$(printf 'id = "%s"\n\n[[changes]]\ncrate = "%s"\nkind = "feature"\ndescription = "Live-check change for the consumed-UID ledger verification."\n' \
  "$LEDGER_UID" "$CRATE")"
tmpidx="$(mktemp)"
GIT_INDEX_FILE="$tmpidx" git read-tree "$tip"
blob="$(printf '%s' "$changeset" | git hash-object -w --stdin)"
GIT_INDEX_FILE="$tmpidx" git update-index --add \
  --cacheinfo "100644,${blob},.changesets/pr-live.toml"
newtree="$(GIT_INDEX_FILE="$tmpidx" git write-tree)"
rm -f "$tmpidx"
rr_tip="$(git commit-tree "$newtree" -p "$tip" \
  -m "test: add live-check changeset for ledger verification")"

# 4. Deploy: dev and stable at tip, release-ready at the changeset-carrying tip,
#    then merge the release PR into stable — the provenance blessing requires.
git push -f "$REMOTE" "${tip}:refs/heads/dev"
git push -f "$REMOTE" "${tip}:refs/heads/stable"
git push -f "$REMOTE" "${rr_tip}:refs/heads/release-ready"
# A release PR between these two branches may already be open from an earlier
# cycle on the fork; GitHub allows one per branch pair, so reuse it.
pr="$(gh pr list -R "$REPO" --base stable --head release-ready --state open \
  --json number --jq '.[0].number // empty')"
if [ -z "$pr" ]; then
  pr="$(gh pr create -R "$REPO" --base stable --head release-ready \
    --title "Release cycle-1 (sandbox ledger check)" \
    --body "Sandbox: merge to exercise consume -> ledger -> bless -> sentinel." \
    | sed 's#.*/##')"
fi
echo "Merging release PR #${pr} (release-ready -> stable)"
gh pr merge "$pr" -R "$REPO" --merge

# 5. Restore protection.
set_enforcement "$DEV_RULESET" active
set_enforcement "$RR_RULESET" active
set_enforcement "$STABLE_RULESET" active

cat <<EOF

Deployed. The release-PR merge into stable fires blessing, then the sentinel.
Watch:
  gh run list  -R ${REPO} --limit 6
  gh run watch -R ${REPO} \$(gh run list -R ${REPO} --workflow 'Blessing (release on stable)' --limit 1 --json databaseId --jq '.[0].databaseId')
Verify (read-only):
  git show ${REMOTE}/stable:.release/consumed-ledger.toml     # should list ${LEDGER_UID}
  git show ${REMOTE}/stable:.changesets/pr-live.toml          # should have consumed_in = "cycle-1"
  git ls-remote --tags ${REMOTE} 'refs/tags/cycle-1' 'refs/tags/${CRATE}-[0-9]*'
  gh pr list -R ${REPO} --base dev --head sync/stable-to-dev --state all
EOF
