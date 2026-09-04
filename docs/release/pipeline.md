# Zaino Release Pipeline

## Status

Authoritative statement of Zaino's branching, gating, versioning, changelog,
and release policy. **Supersedes [zingolabs ADR 003](#relationship-to-adr-003)**
(deprecated). Inherited ADR-003 rules that this document does not change are
reproduced verbatim under [Cross References](#cross-references).

### Revision history

- **2026-08-18 — pipeline redesign (this revision).** Reworked the branch
  model, gate naming, deployment-gate model, release identity/tagging, hotfix protocol,
  and the PR chain. The prior revision (below) described a single advancing
  `rc` branch, "tier 1/2/3" gates, version-named RC branches, and left the
  hotfix protocol as an open question. This revision replaces all of that. The
  reasoning behind each change is recorded inline and collected in
  [Design Rationale & Gotchas](#design-rationale--gotchas) so the *why*
  travels with the *what*.
- **(prior) — periodic release flow.** Resolved ADR 003's deferred cadence and
  RC-validation TODOs. Superseded; kept as
  [ADR-0015](../adr/0015-periodic-release-flow.md).

## Framing Principle

> **Humans land code and changesets. CI derives everything else.**
>
> A contributor's job is to land a change — on `dev` via a normal PR, or as a
> hotfix on `rc` — and to accompany it with a **changeset** describing what it
> changes and at what semver level. From there the machinery is deterministic:
> CI aggregates changesets, derives per-crate version numbers, generates
> changelogs, advances gate frontiers, cuts prerelease tags, and keeps the
> release PR current. **No human ever writes a version number**, and no version
> is ever *estimated* — every version shown is a pure, exact function of the
> changesets accumulated since the last release, re-evaluated on demand.

Everything below serves that principle. Where a rule exists, it exists to keep
the derivation deterministic and to make desync visible before it becomes a
released bug.

## Context

Zaino is developed on a `dev` branch. Features are PRed against `dev`. Anything
that lands on `dev` is effectively scheduled for release, in the order it
landed. We do not cherry-pick from `dev` to cut releases — a release is always
a **prefix** of `dev`'s history (the hotfix path, below, is the sole, contained
exception).

There are 17 publishable crates (`zainod`, `zaino-serve`, `zaino-state`,
`zaino-proto`, `zaino-common`, `zaino-primitives`, `zaino-address`,
`zaino-source`, `zaino-rpc`, `zaino-convert-zebra`, `zaino-source-zebra-rpc`,
`zaino-source-zebra-readstate`, `zaino-source-zebra`, `zaino-consensus`,
`zaino-mempool`, `zaino-mempool-service`, `zaino-status`) and 3 internal-only
(`e2e`, `clientless`, `zaino-testutils`). Each public crate is versioned and
released **independently**. The authoritative, machine-read list of governed
targets is [`relman.toml`](../../../relman.toml) at the repo root; this prose
list mirrors it.

> Some worked examples below predate ADR-0008 (which deleted `zaino-fetch` and
> added the source stack) and name the old 6-crate set. The release
> *mechanism* is crate-count-agnostic; only the illustrative tables are stale.

### Relationship to ADR 003

[Zingolabs ADR 003](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md)
previously stated Zaino's branching, versioning, changelog, public-interface,
and release policy at the level of the broader zingolabs organization. That ADR
explicitly deferred two items: a fixed release cadence ("A stable release
schedule should be set in a later ADR") and the process for creating and
validating release candidates (a TODO in its "Release steps" and an entry in
its "Actions" list). This document resolves both, and revises the branch and
gate model beyond what ADR 003 described.

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

## Two Axes: Test Taxonomy vs. Gates

The most common source of confusion in release discussions is conflating two
independent things:

- **Test taxonomy** — what *kind* of test a test is, divided by intrinsic
  properties: compute cost, infrastructure required, duration, and where the
  test is defined. This axis is stable and physical.
- **Gates** — *promotion checkpoints* in the pipeline. A gate divides by a
  policy question: *at this checkpoint, which tests is it worth running, given
  how often the checkpoint fires and how expensive a defect that slips past it
  would be?*

These do **not** map one-to-one. A single kind of test (notably e2e) can run at
more than one gate, in different subsets. Keeping the axes separate is what lets
us name gates for *what they certify* rather than for *which tests they happen
to run* — because the test membership is allowed to change without renaming the
gate.

### Test taxonomy

Divided by cost / infra / duration / definition-location:

| Type            | Needs external services? | Duration      | Defined                          |
| --------------- | ------------------------ | ------------- | -------------------------------- |
| **unit**        | no                       | ms–s          | within a single module/crate     |
| **integration** | no                       | s             | crates integrating, in-repo      |
| **e2e**         | yes (validator, wallets) | ~minutes–tens | full stack, orchestrated in-repo |
| **deployment**  | yes (live chains, infra) | days          | long deployments on a cluster    |
| **manual**      | human                    | minutes       | operator checklist, at blessing  |

> **Project-jargon note (inherited):** the current codebase historically calls
> "unit tests" everything that needs no external services (i.e. unit +
> integration above) and "integration tests" what this table calls e2e. This
> document uses the taxonomy above; where CI wiring still uses the old words,
> the [named suites](#named-suites) indirection insulates the policy from the
> naming.

### Named suites

A gate does not hard-code a list of tests. It runs a **named suite**, and the
suite's *membership* is defined separately, in one manifest, decoupled from the
gate. The gate is the stable concept ("the `dev`-gate suite"); the contents are
a swappable definition.

- `dev-gate` suite — unit + integration + **fast e2e smoke**
- `rc-gate` suite — the full e2e suite
- `release-gate` suite — the deployment suite
- `bless` checklist — the **manual** suite (human attestation at release time)

Two properties this buys us:

1. **Evolvable without churn.** We can move a test between suites, or tune the
   smoke subset, by editing the manifest — no policy document changes, no gate
   renames, nobody re-anchored to "the gate *is* these exact tests."
2. **Cumulative by construction.** Each gate's suite is a superset of the prior
   gate's guarantees (a commit reaching the `rc-gate` has already cleared the
   `dev-gate`). Suites name the *additional* cost admitted at each step, not the
   total.

The suites are expected to be realized as `cargo nextest` profiles / filtersets
plus a deployment-launch descriptor; the exact selectors are an implementation
detail deferred to the build slice. This document fixes the **indirection**,
not the contents.

**How a gate reads its suite (the indirection, wired).** A gate never names a
workflow or a test mode. It requires a **named signal** — a check-run or commit
status whose context is the suite name (e.g. `rc-gate`, configurable per repo) —
to be `success` on the commit it is admitting. *Whatever* produces that signal
is the swappable membership: a `nextest` workflow, an external runner, or a human
posting a manual attestation all satisfy the same gate identically. Concretely:
`rc-gate` checks for a green `rc-gate` check/status on `dev` HEAD before
advancing; the deployment gate runs a **named WorkflowTemplate** whose content is
the suite. So the gate↔flow-point mapping is fixed in code while the gate↔suite
mapping is pure configuration — the two concerns never touch.

## Gates

Three gates, each named for the branch it admits a commit to. A gate runs a
cumulative, cost-bounded suite; the cost budget rises as the gate fires less
often.

| Gate             | Grants entry to  | Suite                | Fires                          | Budget          |
| ---------------- | ---------------- | -------------------- | ------------------------------ | --------------- |
| **`dev`-gate**   | `dev`            | `dev-gate` suite     | pre-merge, **every push**      | minutes         |
| **`rc`-gate**    | `rc`             | `rc-gate` suite      | **nightly**, on `dev` HEAD     | tens of minutes |
| **`release`-gate** | `release-ready`  | `release-gate` suite | **continuous**, per RC commit  | days            |

The gate a commit has cleared is read directly off **which branch it is on** —
the branches *are* the high-water marks (see [Branch Model](#branch-model)).
There is no separate marker ref to maintain: `dev` = passed `dev`-gate,
`rc` = passed `rc`-gate, `release-ready` = passed `release`-gate.

### The `dev`-gate (pre-merge)

Runs on every PR, pre-merge, and is a required check to merge into `dev`.
Because it is a merge requirement, **`dev` itself is the gate's marker**: every
commit on `dev` has cleared it by construction, so no extra branch or ref is
needed for this gate.

Contents: unit + integration + a **fast e2e smoke** subset. Splitting a bounded
e2e smoke out of the full e2e suite and running it here is deliberate — see
[Gotcha: e2e straddles two gates](#gotcha-e2e-is-not-one-bucket). Two hard
constraints govern what may live in this suite:

- **Wall-clock ceiling.** This gate sits on every contributor's critical path
  on every push. Its total runtime must stay under a fixed budget (target:
  ≤ the unit+integration time, or a fixed ceiling on the order of 10–15
  minutes). A test that cannot fit does not belong here; it waits for the
  `rc`-gate.
- **Non-flaky.** A flaky *blocking* pre-merge check is a worse tax than a slow
  one: it erodes trust and trains people to re-run blindly. Flaky candidates
  belong in the nightly `rc`-gate, not here.

### The `rc`-gate (nightly)

Runs nightly against `dev` HEAD as a **batch**: whatever is on HEAD at run time
is tested as a unit. On pass, HEAD is admitted to the `rc` branch (see
[Promotion](#promotion-flow)) and a deployment run is launched. On fail, the `rc`
frontier does not advance; the team fixes forward on `dev` and the next nightly
attempt tests the new HEAD.

Contents: the full e2e suite. We deliberately do **not** bisect between the last
`rc` and HEAD to find the highest green commit — that adds machinery for
marginal granularity. Batch-and-wait is simpler and preferred to start. (Manual
override exists for emergencies.)

### The `release`-gate (continuous deployment)

Runs the deployment suite — days-long, on live chains — against **`rc`
commits**. The deployment gate is **continuous and per-commit-pinned**: when
the `rc`-gate advances `rc` to a new commit, a deployment run pinned to *that
exact commit* is launched immediately. The frontier keeps moving; each
deployment target is frozen.

- Infra supports 3–4 parallel deployment slots plus a queue.
- **Coalescing rule:** when a slot frees, run the deployment gate on the
  *latest* available `rc` commit, skipping any it leapfrogged. Never spend a
  slot on a commit that is already superseded and has not been through the
  deployment gate.
- On pass, the tested commit is admitted to `release-ready`. On fail, the fix
  goes forward on `dev` (or as a hotfix on `rc`) and a later commit re-runs the
  deployment gate.

The commit that passed the deployment gate is what "cleared all gates" means.
There is no further gate at blessing — see
[Blessing](#blessing-the-only-human-decision).

## Branch Model

Four branches, each the high-water mark of the gate that admits to it:

```
dev  ──►  rc  ──►  release-ready  ──►  stable
 │         │            │                │
 │    passed rc-gate    │           blessed (human)
 │  (in deployment)  passed          published release
passed dev-gate      release-gate
(everything here)    (fully gated,
                      always blessable)
```

- **`dev`** — linear queue of all accepted work; passed the `dev`-gate. Only
  moves forward (fast-forward merges).
- **`rc`** — commits that passed the `rc`-gate and are **under deployment
  testing**. This is what "release candidate" conventionally means: a build
  being validated. Carries the `cycle-*-rc.N` prerelease tags and is the
  **landing zone for hotfixes**.
- **`release-ready`** — commits that passed the `release`-gate (deployment).
  Always fully gated, therefore **always safe to bless**. This is the head of
  the release PR into `stable`.
- **`stable`** (a.k.a. `main`) — the latest published release.

**Why two intermediate branches, not one.** `rc` advances on *nightly* pass so
the deployment gate can start immediately — which means `rc`'s HEAD is routinely
*freshly untested by the deployment gate*. If the release PR pointed at `rc`,
blessing could ship code that never cleared the deployment gate. Graduating
deployment-passed commits to a second branch, `release-ready`, guarantees the
release PR's head is *always* fully gated, so blessing is a pure human timing
decision, never a gamble. `rc → release-ready` is a CI fast-forward on
deployment-pass.

**Branches carry no version in their name — on purpose.** The release version
is *derived* from changesets and is not knowable until blessing (more changesets
may land first). A version-named branch (`rc/0.8.0`) would go stale the moment a
larger bump accrued, forcing a rename. All four branches are
**version-agnostic**; the derived version lives only in the release PR and
prerelease notes. See [Gotcha: version-agnostic
everything](#gotcha-nothing-renameable-carries-a-version).

**Branches only move forward.** After a blessing, `release-ready == stable`;
the next cycle's frontier advances forward from there. This preserves a clean,
append-only lineage that the promotion rules and the hotfix backport depend on.

## Promotion Flow

`dev` is a linear queue that only moves forward. New work keeps landing on
`dev` regardless of gate status; a gate failure blocks **frontier advancement**,
not development.

```
                     nightly rc-gate            continuous release-gate
dev HEAD ───────────────► rc ──(pinned deployment, 3–4 slots)──► release-ready ──► stable
  (dev-gate,              (deploying,                       (deployment-passed, (blessed)
   every push)            cycle-*-rc.N tags)                always blessable)
```

At any moment the three branch tips answer "what is the newest commit that
cleared gate N?" — `dev` for the `dev`-gate, `rc` for the `rc`-gate,
`release-ready` for the `release`-gate. These are the **visible markers** the
pipeline is built to expose.

### Gate failures are fixed forward

When a gate fails, the frontier does not advance; the fix lands on `dev` as a
normal PR (or, when justified, as a [hotfix](#hotfix-protocol) on `rc`), and a
subsequent gate attempt tests the new state. The pipeline never reorders or
reverts shared history.

- **Cost:** a gate-blocking commit delays advancement until a fix lands and the
  next attempt succeeds.
- **Mitigation:** nightly `rc`-gate attempts surface failures within ~24h,
  while author context is fresh.
- **Caveat:** fix-forward targets HEAD, which may have accumulated other work.
  If that work adds *another* problem, the fix must account for both. This is
  the standing pressure to keep gate feedback tight — the less time between a
  bad commit and its detection, the less unrelated work piles on top.

### `dev` is the release queue

Everything on `dev` is meant for release; the machinery *will* try to ship it.
Therefore nothing may land on `dev` that we *already know* isn't releasable.

| Work type                | Belongs on `dev`? | Risk                   |
| ------------------------ | ----------------- | ---------------------- |
| Bug fix                  | Yes               | None                   |
| Completed feature        | Yes               | Might fail higher gate |
| Feature-gated incomplete | Yes               | Inert in default build |
| **Ungated incomplete**   | **No**            | **Poisons the queue**  |
| Refactor (internal)      | Yes               | None                   |
| Refactor (public API)    | Yes               | Downstream breakage    |
| CI/infra (non-src)       | Yes               | Invisible to release   |

**Policy:** ungated incomplete work must never land on `dev`. All incomplete
features must be behind a feature gate. An ungated broken commit doesn't just
risk itself — it **stalls the shared pipeline**: the nightly `rc`-gate fails and
*no one's* frontier advances until it's fixed forward. Every defect caught at
the `dev`-gate (including by the fast e2e smoke) is a queue-stall never paid.

### Crate modularity reduces blast radius

The more crates depend on abstractions (traits) rather than concrete types from
other crates, the more independent PRs are: implementation changes can't
silently break another crate, contract changes are explicit and small,
gate-failing commits are more likely revertable in isolation (kept as an
emergency option, though the flow doesn't rely on reverts), and PRs conflict
less.

## The PR Chain

Standing PRs are used deliberately as **live desync sentinels**: a PR forces its
head branch to stay mergeable into its base, so GitHub itself surfaces — and
demands resolution of — any fix that landed on one side but not the other.

### Forward: the Release PR (`release-ready → stable`)

Long-lived. CI keeps its **description** current with the derived per-crate
version table and the aggregated changelog — i.e. *"merge this and here is
exactly what ships, right now."* Blessing is merging it (see below).

- **Detects:** an emergency hotfix pushed straight to `stable` moves the base
  ahead of the head; GitHub flags the PR out-of-date, forcing the fix back into
  `release-ready` / `rc`.
- **Update discipline:** keep it current by **merge**, never by rebase (see the
  protected-branch rule below). The default "Update branch" (merge) is a
  non-force operation and is safe.

### Reverse: the Backport Sentinel (aux branch → `dev`)

Guarantees no commit that reached `stable` is ever stranded outside `dev` (a
released hotfix, or the release merge itself). Modeled event-driven: a bot
watches for `stable \ dev ≠ ∅` and, when non-empty, prepares the backport and
opens a PR into `dev` with **auto-merge enabled**, so it self-dissolves once
reconciled.

**The PR self-dissolves; it is not manual ceremony.** The backport carries only
already-vetted content (the blessing's release commit, or a hotfix that already
cleared the `rc`- and deployment-gates), so it needs no *review*. The PR exists
for two reasons: to run the `dev`-gate on the **merged result** — catching a
*semantic* conflict a textually-clean merge would hide — and to be a **visible
desync signal** exactly when one is needed. Auto-merge (with a **merge commit**,
never squash/rebase — only a true merge makes `stable \ dev` empty and stops the
sentinel re-firing) delivers both:

- **clean + gate-green** → merges instantly, no human touch, `dev` catches up,
  the sentinel goes quiet;
- **textual conflict *or* red gate** → the PR stays open as the signal a human
  must act on (resolve on the aux branch, or fix the breakage).

So a no-conflict backport is *not* left as a manual PR to click, and it is *not*
direct-pushed past the gate either — it auto-merges through the gate. Choosing
auto-merge over a direct push also avoids putting the App on `dev`'s
protection-bypass list.

**Why not a direct `stable → dev` PR:** keeping such a PR mergeable would mean
rebasing/"update branch"-ing its head — which is `stable`, a protected branch —
and that force-pushes a protected branch, which is forbidden. So the reverse
sync goes through a **disposable auxiliary branch cut from `stable`**: resolve
conflicts there (force-push it freely — it's throwaway), then PR
`sync/stable→dev → dev`. From `dev`'s point of view it's an ordinary feature
PR. Trivial (clean cherry-pick) cases the bot prepares fully; only genuine
conflicts need a human on the aux branch.

### Rule: a protected branch is never a PR *head*

Generalizing the above: a protected branch (`stable`, and a lightly-protected
`release-ready`) may be a **merge target** (things merge into it) and a
**branch source** (you cut from it), but **never a PR head you must keep
mergeable** — because the "Update branch (rebase)" path force-pushes the head.
Any reconciliation that would mutate a protected branch instead happens on a
disposable aux branch. Corollary: **update-by-merge, never update-by-rebase**,
on any protected-ish head.

The intra-pipeline advances — `dev → rc` and `rc → release-ready` — are **CI
fast-forwards on gate-pass**, not human PRs. They can be surfaced as
visibility-only PRs if the pipeline backlog is wanted in the PR list, but the
two sentinels above are the ones doing structural work.

## Release Identity: Versions, Tags, Changesets

Nothing that can only be *renamed-or-recreated* (a branch, a ref) carries a
version. Version identity is **late-bound** and lives in three layers, all
**derived**, never estimated.

### Changesets

Every PR to `dev` (and every hotfix on `rc`) must include a **changeset file**
under `.changesets/`, declaring which crates it affects, at what *semantic*
level (`kind`), with a description. The full field contract, filename scheme,
aggregation, and enforcement live in [Changeset
Format](./changeset-format.md); in brief:

```toml
[[changes]]
crate = "zaino-state"
kind = "feature"   # breaking | feature | fix | internal — CI derives the semver bump
description = "New parallel sync mode"

[[changes]]
crate = "zainod"
kind = "feature"
description = "Expose parallel sync mode in CLI config"
```

- A single PR may declare changes to multiple crates.
- **Enforcement:** CI rejects PRs that touch crate source without a changeset.
- **Per-public-change entries:** when a PR changes a [governed public
  interface](#governed-public-interfaces-inherited-from-adr-003-5), each such
  change is its own `[[changes]]` entry — the aggregated changeset set is the
  source from which per-crate and workspace changelogs are generated, so every
  user-visible change must be listed individually. `description` may be
  multiline and should read as a standalone changelog line (operator-facing,
  plain language, no invented jargon). Internal-only changes still need an
  entry (typically `patch`) but may be collapsed into one describing the net
  effect. This implements the recording requirement of ADR 003 §4 (see [Cross
  References: Changelog policy](#changelog-policy-inherited-from-adr-003-4)).

### Derivation is monotonic within a cycle

CI aggregates all changesets since the last release, resolving the **highest
bump per crate**. Because changesets only *accumulate* within a cycle (never
removed) and aggregation is highest-wins, the derived target version per crate
is **monotonically non-decreasing** across a cycle: `patch → minor → major`,
never backward. It therefore never flaps — the value shown at any instant is
exact for the current changeset set, and the only thing that changes it is more
changesets landing.

On merge of the release PR (blessing), the `.changesets/` directory is cleared;
the next cycle starts fresh.

### Three identity layers

1. **Cycle (period) tags — drive the deliverables.** A release *cycle* (e.g.
   Friday-to-Friday) has an identity independent of any version:
   `cycle-<id>` (e.g. `cycle-2026-08-15`). Prerelease builds within the
   cycle are tagged `cycle-<id>-rc.<N>` and produce prerelease Docker images /
   GitHub prereleases. This is the stable human handle for "the Friday release,"
   and it carries no version — so it can never lie.
2. **Per-crate version tags — crates.io provenance.** At blessing, each crate
   that bumped is tagged `<crate>-<X.Y.Z>` (e.g. `zaino-state-0.4.0`), one git
   point per published `crate@version`. This is standard independent-versioning
   provenance and makes "which commit was `zaino-state 0.4.0` cut from?"
   answerable.
3. **Derived versions — live only in the PR/notes.** The per-crate resolved
   versions appear in the Release PR description and prerelease notes, updated
   by CI, and in *no tag name*. A tester pulling `cycle-…-rc.2` reads the notes
   to learn which crate versions it contains.

**Docker image:** tagged with the daemon (`zainod`) resolved version (e.g.
`zingodevops/zaino:0.5.0`) plus the cycle handle — not a repo-wide `X.Y.Z`,
which is meaningless once crates version independently. (Retains ADR 003 §6's
requirement that images be version-tagged and SHOULD also carry the commit SHA.)

## Blessing: the Only Human Decision

A long-lived **Release PR** (`release-ready → stable`) is the dashboard. Its
head is always fully gated, so it is always safe to merge. CI keeps it showing
recent deployment status, the derived per-crate version table, and the aggregated
changelog. Sketch:

```
## Release Candidate: release-ready @ def456 (cycle-2026-08-15-rc.6)

All gates passed. Merging promotes this commit to stable.

## Deployment status
| RC commit | tag                 | deployment      |
| --------- | ------------------- | --------------- |
| abc123    | cycle-…-rc.7        | day 2/3 running |
| def456    | cycle-…-rc.6        | passed          |
| 789abc    | cycle-…-rc.5        | failed (stall)  |

## Version bumps (derived, since last stable)
| Crate       | Current | Next  | Changes                 |
| ----------- | ------- | ----- | ----------------------- |
| zaino-state | 0.1.0   | 0.2.0 | new sync mode, fix #987 |
| zainod      | 0.2.0   | 0.3.0 | new sync mode exposed   |
```

Releases are **periodic** (e.g. weekly). At the end of a cycle a maintainer, if
satisfied, merges the Release PR. That merge is the **blessing** — and it is the
*only* human decision in the pipeline. It is **not a gate**: all three gates
were already cleared by the commit being promoted. "Whatever cleared all gates
by Friday" is exactly whatever is on `release-ready` on Friday; work that only
cleared the deployment gate after the cut simply ships the next cycle — *easy as that*.

At blessing, CI: finalizes the derived versions, applies the `<crate>-vX.Y.Z`
and `cycle-<id>` tags, publishes (Docker, GitHub Release, crates.io in
dependency order), **marks the shipped changesets consumed** (stamps each
`consumed_in`, rather than deleting — see
[changeset-format.md § Consume by marking](./changeset-format.md#consume-by-marking-not-erasing)),
and the reverse [backport
sentinel](#reverse-the-backport-sentinel-aux-branch--dev) fires to carry the
release commit — bumps, changelogs, and the consumed marks — back into `dev`.

## Hotfix Protocol

The primary flow is fix-forward on `dev`. A **hotfix** is for when a fix is
known, `dev` has diverged enough that landing it there and waiting for a fresh
commit to re-traverse the `rc`- and `release`-gates is too slow, and the fix is
needed in the *current* release line.

**The protocol (one rule):**

1. Land the fix **on `rc`**, with its changeset. It carries an `rc`-gate check
   like anything entering `rc`, then goes through the **deployment gate** like
   any other `rc` commit — hotfixes are not exempt from it.
2. On deployment-pass it graduates to `release-ready` and can be blessed.
3. **Backport to `dev` before `rc`'s next fast-forward advance.** The reverse
   [backport sentinel](#reverse-the-backport-sentinel-aux-branch--dev) enforces
   this: once the hotfix reaches `stable`, `stable \ dev` is non-empty and the
   sentinel demands the backport.

**Why this dissolves the classic hotfix tangle.** The historical worry was that
a hotfix on `rc` makes `rc` diverge from `dev`, so a later "advance `rc` to a
new `dev` commit" would lose the hotfix. That only bites if `rc` advances by
*replacement* to an arbitrary `dev` commit. Here `rc` only ever
**fast-forwards**, and the single rule "backport before next advance" means the
new frontier already contains the fix — so the advance stays a clean
fast-forward and nothing is lost. Version-agnostic branches make this safe:
there is no version-named branch to reconcile.

**Nuclear option.** A true emergency may go straight to `stable`. Both
sentinels then catch the fallout: the forward Release PR flags `release-ready`
as behind `stable` (forcing the fix into `rc`/`release-ready`), and the reverse
sentinel forces it into `dev`.

## Implementation

> The execution-substrate map (GitHub control plane / Argo deployment data plane), the
> `relman` responsibility boundary, and the GitHub↔cluster deployment-gate bridge are
> detailed in [Implementation Architecture](./implementation.md). Summary below.

**Logic lives in a Rust CLI; CI stays thin.** The changeset parse/aggregate,
semver derivation, transitive-bump computation, format-preserving `Cargo.toml`
edits (via `toml_edit` — never `sed`, per the repo's Rust-native rule),
changelog generation, and release-PR body rendering live in a Rust CLI. CI
workflows call it and do only git/`gh` glue. This keeps the derivation
**unit-testable and locally runnable** (the same commands a maintainer can run
by hand), rather than trapped in untestable shell.

**A new sibling tool crate, not an extension of `workbench`.** The existing
`tools/workbench` is a deliberately **std-only, no-framework, one-binary-per-file**
dev-tooling crate in its own isolated workspace, doing heavy lifting by shelling
out (`curl`/`tar`/`diff`/`cargo`/`git`) specifically to avoid pulling in Rust
crates. The release CLI needs the opposite — real dependencies (`toml_edit`,
`semver`, `serde`/`toml`) and a subcommand router (`clap`) so `changeset` /
`bump` / `changelog` operations can share the crate-graph logic. Adding those to
`workbench` would break its documented "tiny, never touch the production graph"
contract. Instead, a **new sibling crate under `tools/`** (e.g. `tools/relman`)
**copies workbench's isolation pattern** — its own workspace, `publish = false`,
fmt/clippy/tested in CI — but is a clap-based multi-subcommand binary carrying
the release deps. That isolation is exactly what licenses using those crates
without affecting production or `cargo-deny`. The existing
`check-published-versions` guard stays in `workbench` and is reused as-is from
the publish flow.

No external release manager (`release-plz`, `cargo-release`, `knope`) is
adopted: our model (version-agnostic branches, cycle tags, continuous deployment,
derived-only versions) diverges enough that config-fighting would cost more than
it saves; ideas may be borrowed, the tool is not.

**Existing-workflow teardown** (first implementation step, before building the
replacement):

| Workflow | Verdict |
| -------- | ------- |
| `auto-tag-rc.yml` | **delete** — derives version from the `rc/<version>` branch name; obsolete under version-agnostic branches |
| `final-tag-on-stable.yml` | **delete** — same branch-name→version coupling; replaced by blessing-time tagging from changesets |
| `release.yaml` | **rework** — retarget to `cycle-*` + `<crate>-vX.Y.Z` tags; Docker image = `zainod` version + cycle handle |
| `publish-dry-run.yml` + `check-published-versions` | **keep / rework** — the Rust guard is reusable; fold into the new publish flow |
| `ci.yml`, `ci-nightly.yaml` | **rework** into the `dev`-gate and `rc`-gate suite runners |
| `compute-tag.yml`, `build-n-push-ci-image.yaml`, `trigger-integration-tests.yml`, `shellcheck.yaml` | **keep** — orthogonal to release versioning |

## Open Questions (deferred to the build slice)

The design above is settled. These *mechanism* details are deferred to
implementation and do not block the branch/gate/identity model:

### Changeset format & generation details (open)

Exact `.changesets/` TOML schema, the aggregation command's changelog-rendering
rules, and the precise shape of the bot commit's edits. The *how* (below) is
decided; these content specifics are for the build slice.

### Transitive version bumps

When crate B bumps and crate A depends on B: if B's bump stays within A's caret
range, A is untouched; if it crosses a compatibility boundary, A's `Cargo.toml`
requirement must update — a source change forcing at least a patch bump on A.
Under Cargo caret semantics the boundaries are: 0.x → any minor is breaking
(0.1→0.2); 1.x+ → only major is breaking; 0.x→1.0 is a boundary. Under current
0.x versioning, every dependency minor bump is a boundary crossing, so
transitive bumps are frequent; reaching 1.0 would reduce the noise.
**Direction (committed):** CI derives transitive bumps **mechanically** during
changeset aggregation — never hand-tracked by PR authors. The exact algorithm
is deferred.

### Dependency version requirement syntax

Tilde (`~1.2.0`) locks to a minor and accepts only patches — tighter than caret
but causes *more* transitive bumps. Trade-off to evaluate once crates stabilize
past 0.x.

### Version targeting

Consensus on per-crate independent versioning; the specific 1.0 timing and
whether crates move in lockstep or independently remains to be defined.

## Design Rationale & Gotchas

The traps and realizations that shaped this revision, kept so the reasoning
survives the decisions.

### Gotcha: e2e is not one bucket

The debate "should e2e run on every feature, or only at RC cut?" dissolves once
you stop treating e2e as atomic. A **fast e2e smoke** subset can be made cheap
and deterministic enough to gate pre-merge (catching cross-service regressions
before they poison the `dev` queue); the **full e2e suite** stays nightly. So
e2e legitimately *straddles* the `dev`-gate and the `rc`-gate. This is why gates
are named for what they certify, not for a test type — "the e2e gate" would be a
category error. (It also restores, more sharply, the fast/full split ADR 003
originally had and the prior revision collapsed.)

### Gotcha: nothing renameable carries a version

Version-naming a branch (`rc/0.8.0`) early-binds the one thing the changeset
system exists to keep late-bound. Since the aggregated bump only grows as
changesets land, a branch named for an early guess goes stale and would need
renaming — messy, and it defeats derivation. Resolution: **all branches and
refs are version-agnostic**; the derived version lives only in the Release PR
and prerelease notes, and is finalized exactly once, at blessing. There is no
"estimated" version anywhere — only a deterministic derivation re-evaluated on
demand.

### Gotcha: the deployment gate needs a frozen target, but freezing the branch is wrong

The deployment gate takes days; if its target moved with the frontier, it would
always be testing stale code. But *freezing a branch* for a period blocks the
"deploy whatever passed, ASAP" goal. Resolution: freeze the **deployment run**
(pin it to a commit + `cycle-*-rc.N` tag), not the branch. The frontier keeps
advancing and launching fresh pinned deployment runs (3–4 slots,
coalesce-to-latest); `release-ready` tracks the newest deployment that passed.
Whatever also had time to clear the deployment gate by Friday ships; the rest
waits.

### Gotcha: a protected branch must never be a PR head

A standing `stable → dev` sentinel is tempting but unworkable: keeping it
mergeable means "update branch," and the rebase variant force-pushes `stable`,
which protection forbids. Resolution: the reverse sync runs through a
**disposable aux branch cut from `stable`**, PR'd into `dev` like any feature —
which is exactly the manual workaround maintainers already used, now formalized
and bot-assisted. Generalized to the rule: protected branches are merge-targets
and branch-sources, never rebase-targets; **update-by-merge, never rebase**.

### Gotcha: naming should say what is certified, not what happened

"Tier 1/2/3" is opaque, and state-past names like "released" are wrong for a
marker that means *releas-able-not-yet-released*. Gates are named for the branch
they **admit to** (`dev`-gate, `rc`-gate, `release`-gate); the branches
themselves double as the gate high-water marks, so no separate marker ref is
needed. `release-ready` says "eligible to ship," not "shipped."

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
`dev`. This repo specifies **four branches** — `dev → rc → release-ready →
stable` (see [Branch Model](#branch-model)). The Release PR into `stable`
originates from `release-ready`; `rc` and `release-ready` are advanced only by
CI fast-forward to gate-passing commits; the "no feature branches directly into
stable" invariant is preserved via the `rc`/`release-ready` intermediaries. The
sole path that reaches `stable` other than the Release PR is the emergency
[nuclear hotfix](#hotfix-protocol), reconciled by the sentinels. Where ADR 003
and this section disagree on the branch graph or PR targeting, this ADR is
authoritative.

**Inherited unchanged**: the 1-CODEOWNER approval requirement for merges into
`dev` and the 2-CODEOWNER approval requirement for merges into `stable`.

### CI test execution (refined by this ADR)

From [ADR 003 §1, "CI / test execution rules"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#1-branch--development-strategy):

> - PRs into `dev`: run a **fast test set** (unit tests where available, small subset of integration tests included while unit tests are missing).
> - Nightly on `dev`: run the **full test suite**.
> - PRs into `stable` (i.e., `dev` → `stable` release PRs): run the **full test suite**.

This ADR refines the two-tier model into three gates ([Gates](#gates)), each
running a [named suite](#named-suites). Mapping:

| ADR 003                          | This ADR                                                       |
| -------------------------------- | -------------------------------------------------------------- |
| Fast test set (PRs into `dev`)   | `dev`-gate — `dev-gate` suite (unit + integration + e2e smoke) |
| Full suite (nightly on `dev`)    | `rc`-gate — `rc-gate` suite (full e2e), nightly                |
| —                                | `release`-gate — `release-gate` suite (deployment), continuous per RC |
| Full suite (`dev → stable` PR)   | **Superseded**: no gate on the blessing merge                  |

**Superseded by this ADR** (gate placement): ADR 003 places a full-suite gate
on the `dev → stable` PR. In this repo, the `release`-gate (deployment) runs against
`rc` commits (days-long each), and its outcome is recorded on the [Release
PR](#blessing-the-only-human-decision). The final merge that promotes
`release-ready` into `stable` is a manual blessing, not a gate — all three gates
have already been cleared by the commit being promoted. Where ADR 003 implies a
re-run of the full suite at merge-into-`stable` time, this ADR is
authoritative: no new suite runs at blessing; blessing is a deterministic
promotion of the most-advanced fully-gated commit.

The `release`-gate is also genuinely new content: ADR 003's single "full suite"
collapsed integration and long-running deployment testing, which is incompatible with gating
a synchronous PR on days-long operations.

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
[Changesets](#changesets)): each governed public-interface change is declared in
its own `[[changes]]` entry, and CI aggregates the changesets accumulated since
the last stable to produce the workspace and per-crate changelogs at release
time. The ZainoDB changelog is maintained separately on the ZainoDB versioning
cadence.

### Governed public interfaces (inherited from ADR 003 §5)

From [ADR 003 §5, "Public interfaces governed by this ADR"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#5-public-interfaces-governed-by-this-adr-and-officially-supported-in-zaino):

> This section defines the "compatibility surface" that drives SemVer bumps and stable-branch gatekeeping.

**Authoritative crate list (this repo)**: [Context](#context) enumerates the
**17 crates.io-published packages** and **3 internal-only packages** (`e2e`,
`clientless`, `zaino-testutils`), mirroring the machine-read
[`relman.toml`](../../../relman.toml). This list has grown since ADR 003:
`zaino-fetch` was **deleted** and the source stack (`zaino-source*`,
`zaino-primitives`, `zaino-address`, `zaino-rpc`, `zaino-convert-zebra`)
**added** by ADR-0008; `integration` was renamed `clientless` by ADR-0004; and
`zaino-consensus`, `zaino-mempool`, `zaino-mempool-service`, `zaino-status` were
added later and brought under governance. The per-crate public-interface
subsections below still reflect the **older** set; deriving the governed
public-item lists for the source-stack, consensus, mempool, and status crates is
a **pending follow-up** (tracked with the drift note at the end of this
section). The release *mechanism* in the body above is unaffected — it operates
over whatever the current crate list is.

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
list governed by this ADR is `e2e`, `clientless`, and `zaino-testutils`.

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

> **Note (this ADR):** `zaino_fetch` was deleted by ADR-0008; its
> responsibilities moved into the source stack. Retained here as the historical
> ADR-003 record until the source-stack crates' governed lists are derived.

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
> - `e2e`
> - `clientless`
>
> These may change freely without affecting SemVer, except where they force changes to governed public crates.

(ADR 003's original excluded list named `integration` — since renamed
`clientless` (ADR-0004) — and `zaino-testvectors`, now in a separate repo with
its own crates.io cadence, out of scope here entirely.)

> **Note** The codebase does not currently reflect this in some places, with entities that should be private currently publicised (or error / config types in the wrong locations). Where this is the case issues / PRs should be opened to provide fixes (make entities pub(crate) or move to the correct location), or a subsequent ADR opened to update the public interface officially maintained.

### Release strategy (superseded by this ADR)

ADR 003 §6 defined release prerequisites, steps, and cadence at a level that
left rc creation/validation and a concrete cadence as open TODOs. Those TODOs
are resolved in the body of this document:

- **Cadence** — [Blessing: the Only Human Decision](#blessing-the-only-human-decision)
- **RC creation and validation** — [Promotion Flow](#promotion-flow), [The `rc`-gate](#the-rc-gate-nightly), [The `release`-gate](#the-release-gate-continuous-deployment)
- **Release steps** — [Blessing: the Only Human Decision](#blessing-the-only-human-decision)
- **Container image publication** — follows ADR 003 §6 step 7: images MUST be tagged with the release version (`vMAJOR.MINOR.PATCH`) and SHOULD also be tagged with the Git commit SHA (see [Release Identity](#release-identity-versions-tags-changesets)).

Source: [ADR 003 §6, "Release strategy"](https://github.com/zingolabs/zingo-adrs/blob/dev/ADR%20003-Zaino%20Branching%2C%20Versioning%2C%20Documentation%2C%20Public%20Interfaces%2C%20and%20Release%20Strategy.md#6-release-strategy).
