# Domain Docs

How the engineering skills should consume this repo's domain documentation when
exploring the codebase. **Layout: multi-context.**

## Before exploring, read these

- **`CONTEXT-MAP.md`** at the repo root — the index of contexts and how they
  relate. Start here; it tells you where each context's glossary lives.
- The **`CONTEXT.md`** for the context you're working in (path from the map).
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence;
don't suggest creating them upfront. The `/domain-modeling` skill (reached via
`/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when
terms or decisions actually get resolved.

## File structure

Multi-context repo (this repo): a root `CONTEXT-MAP.md` points at per-context
`CONTEXT.md` files. A context's glossary is co-located with the code it describes
when it maps to one module, or under `docs/contexts/` when it's a cross-cutting
domain vocabulary.

```
/
├── CONTEXT-MAP.md
├── docs/
│   ├── adr/
│   │   ├── 0001-zcashd-support-feature-gate.md
│   │   └── ...
│   └── contexts/
│       └── shielded-pools/CONTEXT.md      ← cross-cutting domain vocabulary
├── packages/zaino-state/src/chain_index/CONTEXT.md   ← co-located with its module
└── live-tests/CONTEXT.md                              ← co-located with its module
```

zaino is a single Cargo workspace but spans several bounded contexts (shielded
pools, chain index, live tests), so each gets its own glossary and the root map
ties them together. When you add a new context, create its `CONTEXT.md` and add a
line to `CONTEXT-MAP.md`.

**ADR home.** Numbered architecture decision records live in **`docs/adr/`** (e.g.
`0001-zcashd-support-feature-gate.md`). `docs/decision_records/` is a *separate*
area for process notes (the release process) — it is **not** the ADR surface;
don't read or write ADRs there.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a
hypothesis, a test name), use the term as defined in the relevant context's
`CONTEXT.md`. Don't drift to synonyms a glossary explicitly lists under `_Avoid_`.

If the concept you need isn't in any glossary yet, that's a signal — either you're
inventing language the project doesn't use (reconsider) or there's a real gap
(note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than
silently overriding:

> _Contradicts ADR-0001 (zcashd-support feature gate) — but worth reopening
> because…_
