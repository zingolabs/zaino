# ADR 0008: Documentation workbook — per-crate usage guides + coverage ratchet

**Status:** Accepted; the workbook itself is **work in progress** (coverage is
being filled in crate by crate).

## Context

The repo has good documentation *of decisions and behaviour* but no consistent,
task-oriented **"how do I use this?"** layer, and no mechanism keeping such docs in
step with the code. What exists today:

- `docs/adr/` — architecture *decisions* (the *why*).
- `docs/notes/` — design notes.
- top-level `docs/` — *system/service* docs (tutorial, use cases, RPC API).
- `CONTEXT.md` files — glossaries only (canonical terms; explicitly no
  implementation detail or design).
- per-crate `CHANGELOG.md` — *what changed* (with an established discipline that
  code changes update it).
- crate `//!` rustdoc — API reference (`cargo doc`).

Missing is the connective tissue: for a given crate, *how do the pieces fit and how
do I consume them?* The mempool rework surfaced this — the subsystem has an ADR
(why), a lifecycle doc (behaviour) and an audit (performance), but nothing that
walks a consumer through the core vs. coherent-tip APIs.

Two shapes were considered for closing the gap:

1. A single monolithic `docs/workbook.md` that grows section by section.
2. Per-crate usage guides plus a top-level index.

(1) is a merge-conflict magnet, hard to navigate, and stales as one blob. (2) matches
the structure the repo already uses (per-crate `docs/`, as `zaino-mempool` has), is
conflict-friendly, co-locates guides with the code they describe, and lets the
top-level index expose coverage gaps.

## Decision

Adopt a **documentation workbook** made of **per-crate usage guides** with a
top-level index:

- Each crate that exposes a non-trivial API gets `packages/<crate>/docs/usage.md`
  — a task-oriented guide to using that crate's public surface (spawn/consume
  recipes, the model, the contracts). It complements, not replaces, the crate's
  `//!` rustdoc (the canonical API reference) and its `CHANGELOG.md`.
- The **workbook index** lives in the root `README.md` (a "Documentation workbook"
  section). It lists each crate's guide and tracks coverage, marked WIP until the
  workspace is covered.
- **Coverage ratchet** (mirrors the CHANGELOG discipline): when a change adds or
  changes a public capability, add or update its section in that crate's
  `usage.md` — a new section if none fits, otherwise fold it into the relevant
  existing one. Over time this fills the workbook in without a big-bang effort.
  The rule is recorded in `CLAUDE.md` (and thus `AGENTS.md`, its symlink).

Seeded by the mempool subsystem: `packages/zaino-mempool/docs/usage.md` (ports +
types + the change-feed contract) and `packages/zaino-mempool-rpc/docs/usage.md`
(the runtime: spawn core, consume, layer coherence, stream).

## Consequences

- New contributors get a per-crate "how to use it" entry point; the index shows at
  a glance which crates are still undocumented.
- Documentation coverage grows incrementally alongside the code that motivates it,
  rather than as a separate, easily-deferred effort.
- One more artifact to keep current per change — bounded by the same discipline the
  CHANGELOG already carries, and scoped to public-surface changes.
- Guides live beside their crate, so they move/rename/retire with it.

## Follow-ups

- Fill in `usage.md` for the remaining crates (`zaino-state`, `zaino-fetch`,
  `zaino-serve`, `zaino-common`, `zainod`, `zaino-proto`) as they are next touched.
- Consider whether any guide should instead be promoted to crate rustdoc when it is
  purely API-reference material.
