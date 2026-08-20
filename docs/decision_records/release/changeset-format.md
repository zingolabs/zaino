# Changeset Format

> Sub-spec of [pipeline.md](./pipeline.md). Defines the `.changesets/` file
> contract: what a contributor writes, how CI aggregates it into per-crate
> version bumps and changelogs. The CLI that reads/writes these files
> (`tools/relman`) is specified separately; this document is about the **data**,
> not the tool.

## Status

Draft for review. Field design confirmed: entries declare a semantic
[`kind`](#decision-semantic-kind-not-literal-bump) (not a literal semver bump),
refining the illustrative `bump = "minor"` shown in `pipeline.md`. Filenames are
[PR-aligned via a two-phase rename](#file-location--naming).

## What a changeset is

Every PR that changes the source of a governed (published) crate adds **one
changeset file** describing what it changes, per crate, at a semantic level. The
file is the contributor's entire release-facing responsibility: from the
accumulated changesets since the last release, CI derives every crate's version
bump and every changelog line. See [pipeline.md § Framing
Principle](./pipeline.md#framing-principle).

## File location & naming

Files live in `.changesets/` at the repo root, extension `.toml`, **one file per
PR**. The name is **two-phase**, so authoring never blocks on a PR number that
doesn't exist yet:

1. **At authoring time** (before the PR exists) `relman changeset new` creates
   `.changesets/<slug>.toml` with a unique random slug (e.g.
   `wandering-quokka`). Uniqueness means two concurrent PRs add *different* files
   and never merge-conflict.
2. **Once the PR is opened**, a `dev`-gate bot renames the file to
   `.changesets/pr-<N>.toml` (N = the PR number) and pushes the rename to the PR
   branch. PR numbers are inherently unique, so canonical names stay
   conflict-free, and browsing `.changesets/` shows at a glance which PR each
   pending change came from. The rename is idempotent (a no-op once canonical).

The filename is a **readability/audit convention, not a parsed source of
truth**: changelog PR-linking comes from CI's PR context, so a file that hasn't
been renamed yet still links correctly. Fork PRs — where the bot cannot push to
the contributor's branch — simply keep their slug; the check still passes and
linkage still works.

On release, `relman` **marks** the aggregated changesets consumed (it stamps
each with the cycle that shipped it) rather than deleting them. Derivation
filters consumed changesets out, so the next cycle sees an effectively empty set
while `.changesets/` retains the full provenance ledger. See [§ Lifecycle](#lifecycle-read-on-every-derivation-consumed-only-at-release).

## File structure

A changeset is a TOML file containing an array of `[[changes]]` entries. Nothing
else is required.

```toml
[[changes]]
crate = "zaino-state"
kind = "feature"
description = "Add a parallel block-sync mode selectable via config."

[[changes]]
crate = "zainod"
kind = "feature"
description = "Expose the parallel sync mode as a `--sync-mode` CLI flag."
```

A single PR (one file) may declare changes to multiple crates.

### Fields

| Field         | Required | Type   | Meaning                                                             |
| ------------- | -------- | ------ | ------------------------------------------------------------------ |
| `crate`       | yes      | string | A governed (published) crate name. Validated against the workspace. |
| `kind`        | yes      | enum   | Semantic intent of the change (see below). **Not** a literal semver level. |
| `description` | yes      | string | One operator-facing changelog line. Multiline allowed. Plain language, no invented jargon. |
| `section`     | no       | enum   | Keep-a-Changelog section override (`Added`/`Changed`/`Fixed`/`Removed`/`Security`/`Deprecated`). Defaults are derived from `kind`. |
| `migration`   | no       | string | Migration/upgrade notes. Expected on `breaking`; rendered in a "Breaking changes" block. |
| `issues`      | no       | array  | Issue references, e.g. `["#987"]`. The PR number is linked automatically at merge; this is for *additional* refs. |

- `crate` must be a **versioning target declared in `relman.toml`**
  ([Implementation § relman.toml](./implementation.md#relmantoml--the-versioning-target-manifest)) —
  that manifest, not a `cargo metadata` heuristic, is the authority for what is
  governed. Non-target crates (`e2e`, `clientless`, `zaino-testutils`, and any
  crate not listed) are never changeset subjects; a change there that *forces* a
  change in a declared target is recorded against the **target**.
- `description` is written to stand alone as a changelog bullet. Each entry is
  exactly one bullet.

## Decision: semantic `kind`, not literal `bump`

The author declares the **kind of change**, not the resulting version bump:

| `kind`     | Meaning                                                        |
| ---------- | ------------------------------------------------------------- |
| `breaking` | Backward-incompatible change to a governed public interface.  |
| `feature`  | Backward-compatible addition.                                 |
| `fix`      | Backward-compatible bug/perf fix.                             |
| `internal` | No externally observable contract change (refactor, internal). |

CI maps `kind` → a literal semver bump **per crate**, applying the [pre-1.0
relaxation](./pipeline.md#versioning-semantics-inherited-from-adr-003-2) based
on that crate's *current* version:

| `kind`     | post-1.0 crate | pre-1.0 crate (0.y.z) |
| ---------- | -------------- | --------------------- |
| `breaking` | major          | **minor**             |
| `feature`  | minor          | **patch**             |
| `fix`      | patch          | patch                 |
| `internal` | patch          | patch                 |

**Why intent, not literal bump:**

1. **The relaxation is applied exactly once, by the tool.** If authors wrote
   literal `bump = "minor"`, it would be ambiguous whether they already
   discounted for pre-1.0 — and easy to double-apply or forget. Declaring
   `kind = "breaking"` is unambiguous; the tool owns the 0.x policy.
2. **Declarations survive the 1.0 boundary.** A crate crossing 0.x → 1.0 does
   not require rewriting past changesets: the same `kind` simply maps to a
   different bump afterward. Literal bumps would silently mean the wrong thing.
3. **It matches the framing principle** — humans state facts about their change;
   CI derives the numbers.

`internal` additionally marks an entry as **not user-facing**: it forces a patch
bump but is rendered in an "Internal" changelog subsection (or omitted from the
public changelog per rendering config), so a refactor doesn't masquerade as a
user-visible change while still moving the version.

## Per-public-change granularity

When a PR changes more than one [governed public
interface](./pipeline.md#governed-public-interfaces-inherited-from-adr-003-5),
each such change is its **own `[[changes]]` entry** — even for the same crate at
the same `kind`. The aggregated changesets are the source of the changelogs, and
every user-visible change must be listable individually.

Internal-only changes within a governed crate may be **collapsed** into a single
`kind = "internal"` entry describing the net effect.

Transitive bumps are **never authored**: if crate A depends on crate B and B's
bump crosses A's compatibility boundary, CI adds the forced bump on A during
aggregation ([pipeline.md § Transitive version
bumps](./pipeline.md#transitive-version-bumps)). Authors only declare their own
direct changes.

## Aggregation semantics

At any moment (for the release PR body) and at blessing (for the final versions),
`relman` aggregates every changeset in `.changesets/`:

1. **Group** all `[[changes]]` by `crate`.
2. **Resolve the bump per crate**: take the highest `kind`
   (`breaking > feature > fix > internal`), then map to a literal semver bump
   using the crate's current version and the table above.
3. **Add transitive bumps** for dependents whose requirement crosses a
   compatibility boundary (patch, unless a stronger bump already applies).
4. **Collect descriptions** per crate for the per-crate changelog; the union
   feeds the workspace changelog.

Because changesets only accumulate within a cycle and resolution is
highest-wins, each crate's derived bump is **monotonically non-decreasing**
across the cycle — it never flaps. See [pipeline.md § Derivation is
monotonic](./pipeline.md#derivation-is-monotonic-within-a-cycle).

## Lifecycle: read on every derivation, consumed only at release

Aggregation is **read-only and idempotent**. Every prerelease / RC cut, and
every refresh of the release-PR body, re-reads the **entire** current
`.changesets/` set to derive versions and notes — it never removes files. An RC
must be informed by the *whole* set, because an RC is a candidate for *the*
release, and the release's bump is a function of everything accumulated since the
last release; a partially-consumed set would under-count a later RC.

Only a **true release (blessing)** consumes the changesets: after the release PR
merges into `stable`, `relman changeset consume --cycle <N>` stamps each pending
changeset with `consumed_in = "cycle-<N>"`. In short — **prereleases use, the
release consumes** — and consumption happens exactly once per cycle, at the
blessing.

### Consume by marking, not erasing

Consumption **marks** files; it never deletes them. A consumed changeset stays
on disk carrying its `consumed_in` stamp, and every derivation **filters out any
changeset with `consumed_in` set** — exactly as it skips an unfilled template.
Three reasons this beats erasing:

- **Self-defending aggregation.** A released changeset that lingers in
  `.changesets/` (a lagging backport, a stray cherry-pick) is *inert*, not
  silently re-counted. Erasing has no such defense: a present file is always
  counted, so a missed cleanup corrupts the next cycle's derived version — the
  worst failure class, because it is invisible. Marking degrades that to a
  cosmetic stale file.
- **Merge-safe delivery.** The stamp is written on `stable` at the blessing, but
  `dev` is the root of the branch flow (`dev → rc → release-ready → stable`), so
  the stamp — like the version bumps and changelog edits — only reaches `dev`
  via the **stable → dev backport** ([pipeline.md § Hotfix / backport](./pipeline.md)).
  Replaying an *additive* `consumed_in` stamp 3-way-merges cleanly; replaying a
  *deletion* races badly against any changeset `dev` touched in the interim
  (delete/modify conflict). Marking makes the backport forgiving; the backport is
  still what delivers consumption to `dev`.
- **Provenance ledger.** `.changesets/` records which cycle each change shipped
  in; `git log .changesets/pr-<N>.toml` tells the whole story without spelunking
  release tags.

`relman changeset clear` still exists as a manual garbage-collect for pruning old
consumed changesets; it is not part of the release path.

## Changelog rendering

- Default section per `kind`: `feature → Added`, `fix → Fixed`,
  `breaking → Changed` (or `Removed`, via the `section` override),
  `internal → Internal` (or omitted). `section` overrides the default.
- Each `description` becomes one bullet under its crate's changelog and the
  workspace changelog, with the PR auto-linked.
- `breaking` entries additionally render their `migration` note in a
  per-release "Breaking changes" block.
- The ZainoDB on-disk-schema changelog stays on its **own cadence** and is
  **not** driven by these changesets ([pipeline.md § Changelog
  policy](./pipeline.md#changelog-policy-inherited-from-adr-003-4)).

## Enforcement

A `dev`-gate CI check (`relman changeset check`) fails a PR when:

1. The PR changes **source of a declared target** but no changeset entry covers
   it. The check maps changed file paths → owning target using each target's
   `path` in `relman.toml`; every target with changed source must appear in ≥1
   `[[changes]]` entry (of any `kind`).
2. A changeset names a `crate` that is not a declared target, or uses an unknown
   `kind`/`section`.
3. A `[[changes]]` entry is missing a required field.

### Escape hatch: the empty changeset

A PR that touches governed-crate source but is genuinely release-irrelevant
(e.g. a comment-only or test-only change) records an **empty changeset** — a
file with no `[[changes]]` and a required reason:

```toml
# .changesets/<slug>.toml
[empty]
reason = "Comment-only fix in zaino-state; no behavioural or API change."
```

This satisfies enforcement without forcing a spurious patch bump and leaves an
auditable justification. `relman changeset new --empty "<reason>"` creates it.
Empty changesets contribute nothing to versions or changelogs and are marked
consumed like any other on release.

## Worked example

Two PRs land in a cycle. `zaino-state` is at `0.3.1`, `zainod` at `0.4.3`
(both pre-1.0).

`.changesets/wandering-quokka.toml` (PR #1501):

```toml
[[changes]]
crate = "zaino-state"
kind = "breaking"
description = "Replace the `sync()` entrypoint with `sync_with(SyncMode)`."
migration = "Call `sync_with(SyncMode::Serial)` for the previous behaviour."

[[changes]]
crate = "zaino-state"
kind = "fix"
description = "Stop double-counting orphaned blocks in the tip height gauge."
```

`.changesets/brisk-heron.toml` (PR #1509):

```toml
[[changes]]
crate = "zainod"
kind = "feature"
description = "Add a `--sync-mode` flag wiring the new parallel sync mode."
```

Aggregated (pre-1.0 mapping):

| Crate       | Highest kind | Current | Next  | Notes                          |
| ----------- | ------------ | ------- | ----- | ------------------------------ |
| zaino-state | breaking     | 0.3.1   | 0.4.0 | breaking → minor (pre-1.0)     |
| zainod      | feature      | 0.4.3   | 0.4.4 | feature → patch (pre-1.0); plus a transitive check against zaino-state's boundary crossing |

zaino-state's changelog gets both its bullets (the breaking one with its
migration note); zainod's gets one. The workspace changelog gets all three.
