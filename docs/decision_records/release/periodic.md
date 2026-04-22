# Zaino Release Flow Design

## Context

Zaino is developed on a `dev` branch. Features are PRed against `dev`. Anything that lands on `dev` is effectively scheduled
for release, in the order it landed. We do not cherry-pick from `dev` to cut
releases -- the release is always a **prefix** of `dev`'s history.

There are 6 publishable crates (`zainod`, `zaino-serve`, `zaino-state`,
`zaino-fetch`, `zaino-proto`, `zaino-common`) and 2 internal-only
(`integration-tests`, `zaino-testutils`). Each public crate is versioned and
released independently.

## Branch Model

Three branches with a clear lineage:

```
dev ──► rc ──► stable (main)
```

- **`dev`**: linear queue of all accepted work. PRs land via fast-forward merge.
  Only moves forward.
- **`rc`**: advances to specific `dev` commits when an RC is cut. Each RC is
  tagged (e.g. `rc1`, `rc2`). This branch is the moving boundary between
  development and release validation.
- **`stable`** (or `main`): receives merges from `rc` when a release is
  blessed. Represents the latest published release.

```
dev:      C1 ── C2 ── C3 ── C4 ── C5 ── C6 ── C7
                       |                  |
rc:                    C3 (tag: rc1)      C6 (tag: rc2)
                       |
stable:                C3 (rc1 passed all gates, blessed)
```

This structure enables targeted rules for branch-on-branch merges (e.g. `rc`
can only advance to a `dev` commit, `stable` can only merge from `rc`).

## The Pipeline

`dev` is a linear queue that only moves forward. If a commit fails a gate, the
primary response is **fix forward**: land a fix on `dev` and let the line
advance.

At any moment, each gate has a **high-water mark** -- the latest commit on
`dev` that has passed that gate.

```
dev:   C1  C2  C3  C4  C5  C6  C7
        |           |         |
        v           v         v
     tier 3      tier 2    tier 1
     passed      passed    passed

release-ready (latest tier 3 pass) --> C1
latest RC (latest tier 2 pass) ------> C4
dev head -----------------------------> C7
```

New work keeps landing on `dev` regardless of gate status. What a gate failure
blocks is not development, but **gate advancement**.

## Gates

Testing is layered into three tiers. Each tier proves something the previous
one couldn't. A commit must clear all tiers to be releasable.

| Tier | Gate               | Runs when                | What it proves                               |
| ---- | ------------------ | ------------------------ | -------------------------------------------- |
| 1    | Unit tests*        | PR time (pre-merge)      | Local correctness within a crate             |
| 2    | Integration tests* | Nightly advancement      | Cross-crate and cross-service correctness    |
| 3    | Long sync / soak   | On RC cut (tier 2 pass)  | No regressions at scale (full chain, perf)   |

> **\* A note on test naming:** The current Zaino codebase calls "unit tests"
> all tests that don't require launching external services, and "integration
> tests" those that spin up validators, wallets, and orchestrate them with
> zaino. A cleaner taxonomy would be:
>
> - **Unit tests**: within a single module/crate, no cross-crate interaction
> - **Integration tests**: crates integrating with each other, no external services
> - **End-to-end (e2e) tests**: full stack with external services (validator, wallets)
>
> This document uses the current project jargon. The gate labeled "integration
> tests" above corresponds to what would more precisely be called e2e tests.

### Tier 1: Landing on `dev`

A PR must pass unit tests to merge into `dev`. This is the only gate that runs
at PR time. Once merged, the commit is on `dev` and scheduled for release.

### Tier 2: Nightly RC Advancement

Every commit on `dev` has passed tier 1, but not yet tier 2. The nightly
process attempts to **advance** the tier 2 frontier -- the last known commit
that passed integration tests.

The nightly run tests `dev` HEAD against tier 2. Two outcomes:

- **HEAD passes**: it becomes the new RC. The `rc` branch advances to this
  commit, it receives a tag, and it enters tier 3.
- **HEAD fails**: the RC frontier does not advance. The team must land fixes
  (or new work) on `dev` before the next nightly attempt can succeed.

This is a **batch** approach: whatever is on `dev` HEAD at nightly run time is
tested as a unit. This avoids overly frequent RC cutting. If HEAD fails, we
don't bisect to find an intermediate green commit -- we wait for `dev` to
advance. (Manual override is available for emergencies.)

> An alternative strategy would be to bisect between the last known RC and HEAD
> to find the highest green commit. This produces more granular RCs but adds
> machinery. The batch approach is simpler and preferred to start.

### Tier 3: Long Sync / Soak

Only RCs (commits that passed tier 2) enter tier 3. Long sync tests run for
days, validating full chain sync, performance metrics, and stability under
sustained operation.

The infra supports 3-4 parallel long sync slots. When all slots are occupied,
new RCs are queued and begin as slots free up.

```
dev ── nightly attempt ──► RC cut (tier 2 pass) ──► long sync (days) ──► release-ready
        test HEAD            rc branch advances       3-4 slots + queue    tagged
```

### Stable Release: Manual Blessing

A long-lived PR, automatically updated by CI, serves as the release dashboard.
Since an RC only exists because it passed tier 2, the dashboard focuses on
tier 3 status. It tracks:

- Recent RCs and their long sync / soak status
- Changeset-derived version numbers for each crate
- Aggregated changelog
- The **latest RC that passed all gates** -- this is the deterministic release
  candidate (not a recommendation -- it's the most advanced commit that cleared
  everything)

Releases are periodic (e.g. weekly, Friday to Friday). When a maintainer
decides to release at the end of a release period, they merge
this PR into `stable`/`main`. That merge is the blessing.

The PR body might look like:

```
## Release Candidate: RC6 (def456)

All gates passed. Merging this PR promotes RC6 to stable.

## Tier 3 Status

| RC  | Cut from     | Long Sync           |
| --- | ------------ | ------------------- |
| RC7 | C21 (abc123) | day 2/3 running     |
| RC6 | C19 (def456) | passed              |
| RC5 | C15 (789abc) | failed (sync stall) |

## Version Bumps (since last stable)

| Crate        | Current | Next  | Changes                        |
| ------------ | ------- | ----- | ------------------------------ |
| zaino-state  | 0.1.0   | 0.2.0 | new sync mode, fix #987        |
| zaino-serve  | 0.1.0   | 0.1.1 | fix RPC edge case              |
| zaino-fetch  | 0.1.0   | 0.1.0 | (unchanged)                    |
| zaino-proto  | 0.1.0   | 0.1.1 | new message types              |
| zaino-common | 0.1.0   | 0.1.0 | (unchanged)                    |
| zainod       | 0.2.0   | 0.3.0 | new sync mode exposed          |
```

## Changesets: Per-Crate Version Tracking

Every PR to `dev` must include a **changeset file** declaring which crates were
affected and at what semver level, with a short description.

Changeset files live in `.changesets/` and look like:

```toml
[[changes]]
crate = "zaino-state"
bump = "minor"
description = "New parallel sync mode"

[[changes]]
crate = "zainod"
bump = "minor"
description = "Expose parallel sync mode in CLI config"
```

A single PR can declare changes to multiple crates. CI aggregates all changeset
files since last stable, resolves the highest bump per crate, and produces the
version table and changelog in the release PR.

**Enforcement**: CI rejects PRs that touch crate source without including a
changeset file.

On merge of the release PR, changeset files are cleared. The next release
period starts fresh.

## Why This Flow Works

### `dev` Is the Release Queue

Everything that lands on `dev` does so under the premise that it's meant for
release. The nightly machinery **will** try to advance it through the gates and
ship it. Every other feature branch fast-forwards on top of it. There is no
"merge now, decide later" -- if a commit is on `dev`, it's in the release
pipeline.

This means: if something is known in advance to not be ready for release, it
must not merge into `dev`. We can't guarantee in advance that every commit will
pass all gates, but we can guarantee that nothing lands on `dev` that we
*already know* isn't releasable.

Work types and their properties:

| Work type                   | Belongs on `dev`? | Risk                   |
| --------------------------- | ----------------- | ---------------------- |
| Bug fix                     | Yes               | None                   |
| Completed feature           | Yes               | Might fail higher gate |
| Feature-gated incomplete    | Yes               | Inert in default build |
| **Ungated incomplete**      | **No**            | **Poisons the queue**  |
| Refactor (internal)         | Yes               | None                   |
| Refactor (public API)       | Yes               | Downstream breakage    |
| CI/infra (non-src)          | Yes               | Invisible to release   |

**Policy**: ungated incomplete work must never land on `dev`. All incomplete
features must be behind a feature gate.

### Gate Failures Are Fixed Forward

When `dev` HEAD fails the nightly tier 2 attempt, the RC frontier does not
advance. The fix lands on `dev` as a normal PR, and the next nightly attempt
tests the new HEAD. The pipeline does not reorder or revert. This is
deliberately simple -- no one needs to reason about revert cascades or rebasing
shared history.

Similarly, if an RC fails tier 3 (long sync), the fix lands on `dev`, and a
future nightly run produces a new RC that includes the fix.

The cost: a gate-blocking commit delays frontier advancement until a fix lands
on `dev` and a subsequent nightly attempt succeeds. The mitigation: nightly
attempts mean failures are surfaced within ~24 hours, while the author's context
is fresh.

**Caveat**: fixing forward targets `dev` HEAD, which may have accumulated
additional work since the failing commit. If that additional work introduces
*another* problem, the fix must account for both -- or the next nightly attempt
fails for a different reason. This is the main pressure to keep gate feedback
tight: the less time between a bad commit landing and its detection, the less
unrelated work piles on top, and the simpler the fix.

### Crate Modularity Reduces Blast Radius

The more decoupled the crates are, the more independent PRs tend to be. When
modules depend on abstractions (traits) rather than concrete types from other
crates:

- Implementation changes in one crate can't silently break another
- Contract changes (trait modifications) are explicit, small, and obvious
- Gate-failing commits are more likely to be revertable in isolation (even
  though the flow doesn't rely on reverts, the option exists for emergencies)
- PRs are less conflict-prone, reducing rebase friction

## Open Questions

### Transitive Version Bumps

When crate B bumps its version and crate A depends on B, does A need a bump
too -- even if A's source code didn't change?

If B's bump stays within A's declared compatibility range, A's `Cargo.toml`
doesn't change and no bump is needed. If B crosses a compatibility boundary,
A's `Cargo.toml` **must** update the version requirement. That's a source
change, which forces at least a patch bump on A.

Under Cargo's default caret semantics, compatibility boundaries are:

- **0.x**: any minor bump is breaking (0.1 → 0.2 crosses the boundary)
- **1.x+**: only major bumps are breaking (1.2 → 1.3 is compatible, 1.x → 2.0
  crosses the boundary)
- **0.x → 1.0**: also a boundary crossing

Under the current 0.x versioning, every minor bump in a dependency is a
compatibility boundary crossing, which means frequent transitive bumps. Reaching
1.0 on stable crates would reduce this noise.

This could be automated as part of changeset aggregation rather than relying on
PR authors to track it manually.

### Dependency Version Requirement Syntax

Cargo supports tilde requirements (`~1.2.0` = `>=1.2.0, <1.3.0`) which lock
to a specific minor version and only accept patches. This is tighter than the
default caret (`^1.2.0` = `>=1.2.0, <2.0.0`).

For workspace-internal dependencies, tilde gives more control but causes
**more** transitive bumps (even a minor bump in a dependency forces a
`Cargo.toml` update in dependents). Trade-off to evaluate once crates
stabilize past 0.x.

### Hotfix Protocol

The primary flow is fix-forward on `dev`. But there may be cases where an RC
has failed tier 3, a fix is known, and the current `dev` HEAD has diverged
enough that landing the fix on `dev` and waiting for a new RC to re-traverse
tiers 2 and 3 is too slow.

A "hotfix" would target the RC directly on the `rc` branch, bypassing `dev`.
This raises unresolved questions:

- **Backporting**: the hotfix must eventually reach `dev`, or future RCs carry
  the original bug. If `dev` has diverged, the backport may conflict or need
  adaptation, producing two versions of the same fix.
- **Linearity**: once a hotfix lands on `rc` but not on `dev`, the branches
  diverge. The `rc` branch is no longer a clean pointer into `dev`'s history.
  New RC cuts from `dev` must reconcile this.
- **Cascading**: if a hotfixed RC is released to `stable`, and `stable` merges
  back, the hotfix commit exists in `stable` but not in `dev`. Future merges
  need to handle the divergence.
- **Blocking new RCs**: if the hotfix on `rc` doesn't also land on `dev`, can
  new RCs even be cut? The `rc` branch has a commit `dev` doesn't know about.
  Advancing `rc` to a new `dev` commit would lose the hotfix.

Each question has answers, but each adds a rule the team must internalize. This
is deferred until fix-forward proves insufficient in practice.

### Version Targeting

The team has consensus on per-crate independent versioning, but the specific
version targeting strategy (when to go 1.0, whether all crates move in lockstep
or independently) remains to be defined.

