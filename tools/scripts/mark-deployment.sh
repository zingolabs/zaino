#!/usr/bin/env bash
# Simulate the deployment-gate's result WITHOUT running a real (days-long)
# deployment. Creates a GitHub Deployment for an RC commit and sets its
# deployment_status — exactly the signal the cluster's Argo Workflow would post
# on completion. This lets you test the pipeline's REACTION (advance
# release-ready, refresh the release PR, enable blessing) in seconds instead of
# days. The real endurance run is a separate, infra-side concern.
#
# See docs/decision_records/release/implementation.md
# § "Testability: never wait days to test the pipeline".
#
# Usage:
#   tools/scripts/mark-deployment.sh <owner/repo> <ref|sha> <state> [environment]
#     state:       success | failure | in_progress | error | inactive | queued | pending
#     environment: defaults to "deployment"
#
# Requires: gh (authenticated, repo scope) and jq.
set -euo pipefail

REPO="${1:?usage: mark-deployment.sh <owner/repo> <ref> <state> [environment]}"
REF="${2:?ref or sha of the RC commit}"
STATE="${3:?state: success|failure|in_progress|error|inactive|queued|pending}"
ENVIRONMENT="${4:-deployment}"

case "$STATE" in
  success|failure|in_progress|error|inactive|queued|pending) ;;
  *) echo "invalid state: $STATE" >&2; exit 2 ;;
esac

# 1. Create a Deployment for the commit. required_contexts:[] so it is not gated
#    on status checks; the deployment run itself is what we are simulating.
dep_id="$(jq -n --arg ref "$REF" --arg env "$ENVIRONMENT" \
  '{ref:$ref, environment:$env, auto_merge:false, required_contexts:[],
    description:"simulated deployment-gate run (mark-deployment.sh)"}' \
  | gh api "repos/$REPO/deployments" -X POST --input - --jq '.id')"
echo "created deployment $dep_id (env=$ENVIRONMENT, ref=$REF)"

# 2. Post the deployment_status — the callback the cluster sends on finish.
#    The release-ready-advance workflow (bridge step 6) reacts to this event.
state_out="$(jq -n --arg state "$STATE" --arg env "$ENVIRONMENT" \
  '{state:$state, environment:$env, description:("simulated: "+$state)}' \
  | gh api "repos/$REPO/deployments/$dep_id/statuses" -X POST --input - --jq '.state')"
echo "deployment_status = $state_out"
echo "done — the deployment-gate reaction workflow should now fire on this deployment_status."
