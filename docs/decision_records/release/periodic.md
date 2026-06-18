# Zaino Release Flow Design

## Context

Zaino is developed on a `dev` branch. Features are PRed against `dev`. Anything that lands on `dev` is effectively scheduled
for release, in the order it landed. We do not cherry-pick from `dev` to cut
releases -- the release is always a **prefix** of `dev`'s history.

There are 6 publishable crates (`zainod`, `zaino-serve`, `zaino-state`,
`zaino-fetch`, `zaino-proto`, `zaino-common`) and 2 internal-only
(`integration-tests`, `zaino-testutils`). Each public crate is versioned and
released independently.

### Relationship to ADR 003

[Zingolabs ADR 003](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md)
previously stated Zaino's branching, versioning, changelog, public-interface,
and release policy at the level of the broader zingolabs organization. That ADR
explicitly deferred two items: a fixed release cadence ("A stable release
schedule should be set in a later ADR") and the process for creating and
validating release candidates (a TODO in its "Release steps" and an entry in
its "Actions" list). This document resolves both.

**Governance principle**: a decision record versioned alongside the code it
governs is authoritative over a decision record held in a separate, generic
repository. Release policy, branching rules, and public-interface governance
are only meaningful relative to a specific state of the code; divorcing them
from the `Cargo.toml`, `CODEOWNERS`, and crate graph they constrain makes the
policy impossible to evolve coherently (a change to the governed public-item
list in one repo has no way to land atomically with the code change it
describes in another). This ADR therefore **supersedes ADR 003** as the
authoritative statement of Zaino's branching, versioning, public-interface,
changelog, and release policy. ADR 003 is **deprecated**; the text this
document inherits from it is reproduced verbatim under [Cross
References](#cross-references) with per-section back-references to the
original. Future changes to any of these rules should be made here, not in
zingo-adrs.

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

This three-tier model refines the two-tier fast/full split ADR 003 originally
prescribed; the mapping and the reasoning for splitting long-sync validation
into its own tier are documented in [Cross References: CI test
execution](#ci-test-execution-refined-by-this-adr).

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

**Per-public-change entries**: when a PR changes a [governed public
interface](#governed-public-interfaces-inherited-from-adr-003-5), the changeset
must include a separate `[[changes]]` entry for each such change. Bundling
multiple public-interface changes under a single entry is not permitted,
because the aggregated set of changesets since last stable is the source from
which the workspace and per-crate changelogs are generated, and every
user-visible change must appear as its own entry so it can be listed
individually. The `description` field may be a multiline string and should be
written to stand alone as a changelog line (operator-facing, plain language, no
invented jargon). Internal-only changes within a governed crate still require a
changeset entry (typically a `patch` bump) but may be collapsed into a single
entry describing the net effect. This implements the recording requirement of
ADR 003 §4 (see [Cross References: Changelog
policy](#changelog-policy-inherited-from-adr-003-4)).

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

## Cross References

This ADR inherits a body of rules from [zingolabs ADR
003](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md).
The inherited text is reproduced here verbatim so that the authoritative
statement of each rule travels with the code it governs. Each subsection
attributes the source section of ADR 003.

**ADR 003 is deprecated** by this document, per the governance principle in
[Relationship to ADR 003](#relationship-to-adr-003): a repo-bound,
version-bound decision record supersedes a generic cross-repo decision record
on matters specific to this repo. Future changes to any rule below must be
made in this file, not in `zingolabs/zingo-adrs`.

### Branching and approvals (inherited from ADR 003 §1)

From [ADR 003 §1, "Branch / development strategy"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#1-branch--development-strategy):

> **Branches**
> - `dev`: primary development branch (default branch).
> - `stable`: release branch (only release-quality changes land here).
>
> **PR targeting rules**
> - PRs may target `dev` directly.
> - PRs may target `stable` **only if they are merges from `dev`** (i.e., *no feature branches directly into stable*).
>
> **Review rules**
> - Merge into `dev`: **1 approval** from CODEOWNERS.
> - Merge into `stable`: **2 approvals** from CODEOWNERS.

**Superseded by this ADR** (branch model and PR targeting): ADR 003 specifies
two branches (`dev`, `stable`) with PRs into `stable` coming directly from
`dev`. This repo specifies **three branches** — `dev → rc → stable` (see
[Branch Model](#branch-model)). PRs into `stable` originate from `rc`, and
`rc` is itself advanced only to commits on `dev`; the "no feature branches
directly into stable" invariant is preserved via the `rc` intermediary. Where
ADR 003 and this section disagree on the branch graph or PR targeting, this
ADR is authoritative.

**Inherited unchanged**: the 1-CODEOWNER approval requirement for merges into
`dev` and the 2-CODEOWNER approval requirement for merges into `stable`.

### CI test execution (refined by this ADR)

From [ADR 003 §1, "CI / test execution rules"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#1-branch--development-strategy):

> - PRs into `dev`: run a **fast test set** (unit tests where available, small subset of integration tests included while unit tests are missing).
> - Nightly on `dev`: run the **full test suite**.
> - PRs into `stable` (i.e., `dev` → `stable` release PRs): run the **full test suite**.

This ADR refines the two-tier model into three tiers. Mapping:

| ADR 003                              | This ADR                                                   |
| ------------------------------------ | ---------------------------------------------------------- |
| Fast test set (PRs into `dev`)       | Tier 1 (unit tests, pre-merge into `dev`)                  |
| Full suite (nightly on `dev`)        | Tier 2 (integration / e2e tests, nightly)                  |
| —                                    | Tier 3 (long sync / soak, runs against each RC)            |
| Full suite (`dev → stable` PR)       | **Superseded**: no gate on the blessing merge              |

**Superseded by this ADR** (gate placement): ADR 003 places a full-suite gate
on the `dev → stable` PR. In this repo, tier 3 runs against an RC on the `rc`
branch (days-long per RC), and its outcome is recorded on the [release
dashboard](#stable-release-manual-blessing). The final merge that promotes an
RC into `stable` is a manual blessing, not a gate — all three tiers have
already been cleared by the RC being promoted. Where ADR 003 implies a
re-run of the full suite at merge-into-`stable` time, this ADR is
authoritative: no new suite runs at blessing; blessing is a deterministic
promotion of the most-advanced RC that cleared every tier.

Tier 3 is also genuinely new content: ADR 003's single "full suite"
collapsed integration and long-sync testing, which is incompatible with
gating a synchronous PR on days-long operations.

### Dependency policy (inherited from ADR 003 §1)

From [ADR 003 §1, "Dependency rules"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#1-branch--development-strategy):

> All non-test dependencies must be crates.io imports on stable.
> Dev may temporarily use feature branches via `[patch.crates-io]`.

### Versioning semantics (inherited from ADR 003 §2)

From [ADR 003 §2, "Versioning strategy (SemVer)"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#2-versioning-strategy-semver-and-what-it-means-in-zaino):

> Zaino follows **Semantic Versioning (SemVer)**: `MAJOR.MINOR.PATCH`.
>
> **Scope choice**
> - Zaino versions are treated as **crate-specific** meaning each publishable crates in this repository will have an individual version number which will be bumped when changes to that repo necessitate it.
>
> **Definitions for Zaino**
> - **MAJOR**: any *backward-incompatible* change to a governed public interface (see "Public interfaces" section), including:
>   - breaking changes to gRPC service behavior/requests/responses,
>   - removing or changing semantics/signatures of public Rust items intended for external users,
>   - breaking configuration/CLI contract for `zainod` where it impacts operators in a non-compatible way.
> - **MINOR**: backward-compatible feature additions, including:
>   - new RPC endpoints/services added without breaking existing ones,
>   - new fields added in a backward-compatible way (where supported by the protocol/encoding),
>   - new public Rust APIs that do not break old ones.
> - **PATCH**: backward-compatible bug fixes, performance fixes, and internal refactors with no externally observable contract change.

**Pre-1.0 relaxation**, from the same section:

> While Zaino remains in the 0.y.z phase, version bumps will be treated as one level "less critical" than post-1.0.0. Specifically, changes that would normally require a major bump will instead require a minor bump, and changes that would normally require a minor bump will instead require a patch bump. Patch bumps keep the same meaning as post-1.0.0.

**ZainoDB versioning**, from the same section:

> - **MAJOR**: Distinct database implementations, providing differing sets of functionality (Currently V1 is the only supported major version. A lightweight V2 database that only holds the minimal set of data required to produce the extra indexes (compared to zebrad) required in Zaino is planned but not yet implemented. The legacy V0 local-cache schema has been removed: an on-disk V0 database is no longer opened or migrated — it is rejected with an error directing the operator to resync a V1 database).
> - **MINOR**: Updates that contain changes to either the public APIs or the on disk schema.
> - **PATCH**: Internal bug fixes / performance improvements that do not touch the public APIs or on disk schema.
>
> Due to this, version changes in ZainoDB may not dictate a change of the same type at the library level.

### Documentation publication (inherited from ADR 003 §3)

From [ADR 003 §3, "GitHub Pages + crates.io documentation update strategy"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#3-github-pages--cratesio-documentation-update-strategy):

> **Docs targets**
> - **GitHub Pages (gh-pages)**: the canonical "workspace documentation" site.
> - **docs.rs (crates.io)**: Rust API docs are automatically built for crates published to crates.io.
>
> **Update rules**
> - Every time `stable` is updated as part of a release (and crates.io is updated), **GitHub Pages MUST be updated** to match that release state.
> - docs.rs updates automatically when crates are published to crates.io.

Implementation via `actions/deploy-pages` as part of the release workflow is
currently unimplemented; manual update of gh-pages at release time is required
until that is automated.

### Changelog policy (inherited from ADR 003 §4)

From [ADR 003 §4, "Changelog policy"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#4-changelog-policy):

> **Changelog locations**
> - **Workspace changelog:** one primary changelog for the repository/workspace (covers cross-cutting changes and release-level summaries).
> - **Per-crate changelogs:** each publishable crate maintains its own changelog for crate-specific changes.
> - **ZainoDB changelog:** ZainoDB maintains an additional database-specific changelog, following the ZainoDB versioning policy defined in this ADR (separate from the crate/workspace SemVer policy).
>
> **What must be recorded**
> - Any change to a governed **public interface** (as defined in this ADR) must be recorded in:
>   - the **workspace changelog**, and
>   - the **relevant crate's changelog**.
> - Any change that affects the **ZainoDB on-disk schema** or database behaviour covered by the ZainoDB versioning policy must be recorded in the **ZainoDB changelog**, and does not necessarily imply a crate/workspace version bump of the same type.

This ADR implements the recording mechanism via changesets (see
[Changesets](#changesets-per-crate-version-tracking)): each governed
public-interface change is declared in its own `[[changes]]` entry, and CI
aggregates the changesets accumulated since the last stable to produce the
workspace and per-crate changelogs at release time. The ZainoDB changelog is
maintained separately on the ZainoDB versioning cadence.

### Governed public interfaces (inherited from ADR 003 §5)

From [ADR 003 §5, "Public interfaces governed by this ADR"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#5-public-interfaces-governed-by-this-adr-and-officially-supported-in-zaino):

> This section defines the "compatibility surface" that drives SemVer bumps and stable-branch gatekeeping.

**Authoritative crate list (this repo)**: [Context](#context) enumerates the
**6 crates.io-published packages** — `zainod`, `zaino-serve`, `zaino-state`,
`zaino-fetch`, `zaino-proto`, `zaino-common` — and **2 internal-only
packages** — `integration-tests` and `zaino-testutils`.

`zainodlib` exists as a library target inside the `zainod` package
(`packages/zainod/Cargo.toml`: `[[bin]] name = "zainod"` alongside
`[lib] name = "zainodlib"`). It is **not** a first-class crates.io-published
package: it has no independent version number and is not `cargo publish`ed
separately. External consumers who import `zainodlib` do so by depending on
the `zainod` package. ADR 003 treats `zainodlib` as a distinct governed
interface surface, and its public-item list remains in force (below), but
its SemVer bumps are expressed through the `zainod` package version, not an
independent version of its own. Changes to `zainodlib`'s public API are
therefore recorded as governed public-interface changes on the `zainod`
crate for changeset purposes.

`zaino-testvectors` is not in this repo. It has been extracted to a separate
repository/workspace and is now published independently to crates.io; its
release policy is governed there, not here. ADR 003's listing of it as an
excluded crate in this repo is therefore moot — it is out of scope entirely
for this ADR. The excluded (internal-only, not-crates.io-published) crate
list governed by this ADR is the two packages named above.

The per-crate subsections below reproduce the public-interface and
public-item lists from ADR 003 verbatim. Subsection headers use the Rust
module form (underscore) to match ADR 003's original headings; the
corresponding package names (`Cargo.toml`) use the hyphenated form.

#### `zainod` (daemon)

> Public interfaces:
> - Zainod daemon: Main indexing daemon
>   - Zcash JsonRPC service
>   - Zcash LightClient gRPC service
>
> Public items:
> - CLI arguments
> - Config format
> - RPC Specs

#### `zainodlib` (daemon library)

> Public interfaces:
> - `indexer::Indexer`: Full indexing server
>
> Public items:
> - `config::*`
> - `error::*`

#### `zaino_serve` (gRPC + JsonRPC servers)

> Public interfaces:
> - `server::{grpc::TonicServer, jsonrpc::JsonRpcServer}`: gRPC / JsonRPC server implementations
>
> Public items:
> - `rpc::{GrpcClient, JsonRpcClient}`
> - `rpc::jsonrpc::service::ZcashIndexerRpc`
> - `server::config::*`
> - `server::error::*`

#### `zaino_state` (core indexing library)

> Public interfaces:
> - `chain_index::source::ValidatorConnector`: Validator agnostic Chain data fetch service
> - `chain_index::{NodeBackedChainIndex, NodeBackedChainIndexSubscriber}`: Core chain indexing service
> - `backends::{fetch::{FetchService, FetchServiceSubscriber}, state::{StateService, StateServiceSubscriber}}`: Indexing API (IndexerService / IndexerSubscriber) based on the zcash RPC services for compatibility, utilising Zaino's underlying indexing services
>
> Public items:
> - `indexer::{IndexerService, ZcashService, IndexerSubscriber, ZcashIndexer, LightWalletIndexer, LightWalletService}`
> - `chain_index::{ChainIndex, NonFinalizedSnapshot}`
> - `chain_index::source::{BlockchainSource, State, BlockchainSourceResult}`
> - `chain_index::encoding::*`
> - `chain_index::types::*`
> - `status::*`
> - `stream::*`
> - `config::*`
> - `error::*`
> - ZainoDB's on disk schema.

#### `zaino_fetch` (Zcash-specific JsonRPC client + parsing)

> Public interfaces:
> - `jsonrpc::connector::JsonRpcConnector`: Zcash specific JsonRPC client with full chain data fetch and block / transaction parsing capability
>
> Public items:
> - `chain::utils::ParseFromSlice`
> - `chain::transaction::*`
> - `chain::block::*`
> - `chain::error::*`
> - `jsonrpc::connector::test_node_and_return_url`
> - `jsonrpc::response::*`
> - `jsonrpc::error::*`

#### `zaino_proto` (LightClient protocol implementation)

> Public items:
> - `::*`

#### `zaino_common` (common types + utilities)

> Public items:
> - `::*`

#### Excluded (not governed)

> - `zaino-testutils`
> - `integration-tests`
>
> These may change freely without affecting SemVer, except where they force changes to governed public crates.

(ADR 003's original excluded list also named `zaino-testvectors`; that crate
now lives in a separate repo with its own crates.io publication cadence, so
it is out of scope for this ADR entirely rather than "excluded" from it.)

> **Note** The codebase does not currently reflect this in some places, with entities that should be private currently publicised (or error / config types in the wrong locations). Where this is the case issues / PRs should be opened to provide fixes (make entities pub(crate) or move to the correct location), or a subsequent ADR opened to update the public interface officially maintained.

### Release strategy (superseded by this ADR)

ADR 003 §6 defined release prerequisites, steps, and cadence at a level that
left rc creation/validation and a concrete cadence as open TODOs. Those TODOs
are resolved in the body of this document:

- **Cadence** — [Stable Release: Manual Blessing](#stable-release-manual-blessing)
- **RC creation and validation** — [The Pipeline](#the-pipeline), [Tier 2](#tier-2-nightly-rc-advancement), [Tier 3](#tier-3-long-sync--soak)
- **Release steps** — [Stable Release: Manual Blessing](#stable-release-manual-blessing)
- **Container image publication** — follows ADR 003 §6 step 7 verbatim: images MUST be tagged with the release version (`vMAJOR.MINOR.PATCH`) and SHOULD also be tagged with the Git commit SHA.

Source: [ADR 003 §6, "Release strategy"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#6-release-strategy).

