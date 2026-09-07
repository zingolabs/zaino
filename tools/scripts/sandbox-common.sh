#!/usr/bin/env bash
# Shared defaults and helpers for the sandbox-*.sh drivers. Source it; never
# run it. Every value can be overridden from the environment.
#
# SENSITIVE — the helpers toggle branch-protection rulesets and delete tags and
# releases. Safe only because REPO is a throwaway fork; never point it at a real
# repository.

REMOTE="${REMOTE:-sandbox}"
REPO="${REPO:-nachog00/zaino-pipeline-sandbox}"
# Ruleset ids for this sandbox (see: gh api repos/$REPO/rulesets).
DEV_RULESET="${DEV_RULESET:-21067396}"
RC_RULESET="${RC_RULESET:-21067398}"
RR_RULESET="${RR_RULESET:-21067399}"
STABLE_RULESET="${STABLE_RULESET:-21067400}"

# The tags the release pipeline creates: `cycle-<N>`, `cycle-<N>-rc.<M>`, and
# the per-crate `<crate>-X.Y.Z` provenance tag (no `v`).
PIPELINE_TAG_PATTERN='^cycle-|-[0-9]+\.[0-9]+\.[0-9]+$'

set_enforcement() { # <ruleset-id> <active|disabled>
  gh api -X PUT "repos/${REPO}/rulesets/${1}" -f "enforcement=${2}" \
    --jq '.name + " -> " + .enforcement'
}

list_pipeline_tags() { # every pipeline tag on the remote, one per line
  git ls-remote --tags "$REMOTE" \
    | sed -n 's#.*refs/tags/\([^^]*\)$#\1#p' | grep -E "$PIPELINE_TAG_PATTERN" | sort -u || true
}

delete_pipeline_tags_and_releases() { # clean slate: no cycle/rc/crate tags, no GitHub Releases
  local tags t id
  mapfile -t tags < <(list_pipeline_tags)
  for t in "${tags[@]}"; do
    echo "  del tag $t"; git push --quiet "$REMOTE" ":refs/tags/$t" || true
  done
  for id in $(gh api "repos/${REPO}/releases" --jq '.[].id'); do
    echo "  del release $id"; gh api -X DELETE "repos/${REPO}/releases/${id}" || true
  done
}
