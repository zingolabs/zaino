# Releases derive from changesets through a four-branch gated pipeline

## Status

accepted — supersedes [ADR-0015](0015-periodic-release-flow.md) (and, through
it, zingolabs ADR 003). The machinery lands inert behind the repo variable
`RELMAN_PIPELINE_ACTIVE` and activates at a deliberate cutover.

## Context and decision

Releases were manual: hand-bumped versions across the workspace, hand-written
changelogs, hand-cut tags, and a dependency-ordered manual publish of the 17
governed crates. Each step could drift from the others with nothing to detect
it. The periodic flow (ADR-0015) fixed cadence and RC validation but kept
version-named `rc/*` branches and manual version derivation.

Decision: a contributor lands each PR with a **changeset** — a TOML file
naming the crates changed and the semver kind of each change. CI derives
everything else: per-crate version bumps, changelog entries, tags, and the
publish plan. No human writes a version number. Code advances through four
fixed, version-agnostic branches — `dev → rc → release-ready → stable` — with
a named quality gate at each admission. The one human release action is the
**blessing**: the merge to `stable`. Derivation is owned by the `relman` CLI
(deterministic, no network or ref mutation); workflow YAML is a thin shell.
Released changesets are consumed by marking, never deleted, so `.changesets/`
is a provenance ledger and stale entries are inert.

## Consequences

- Contributors must write a changeset per governed change (advisory until
  `RELMAN_ENFORCE_CHANGESETS` is set).
- Version-named `rc/*` branches and the legacy auto-tag workflows are gone.
- Release credentials concentrate in CI. The inventory, exposure posture, and
  release-advance paths are recorded in
  [docs/release/implementation.md](../release/implementation.md) § Trust
  model; replacing the stored crates.io token with Trusted Publishing (OIDC)
  is an open item there.
- The full specifications live in `docs/release/`:
  [pipeline.md](../release/pipeline.md) (policy),
  [changeset-format.md](../release/changeset-format.md) (contributor
  contract), [implementation.md](../release/implementation.md) (architecture).
