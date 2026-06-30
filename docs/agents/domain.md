# Domain Docs

How the engineering skills should consume this repo's domain documentation when
exploring the codebase. **Layout: single-context.**

## Before exploring, read these

- **`CONTEXT.md`** at the repo root.
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence;
don't suggest creating them upfront. The `/domain-modeling` skill (reached via
`/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when
terms or decisions actually get resolved.

## File structure

Single-context repo (this repo):

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-zcashd-support-feature-gate.md
│   └── 0002-another-decision.md
└── ...
```

zaino is a single Cargo workspace (the root `Cargo.toml` holds both the
`packages/*` production crates and the `live-tests/*` crates as members) serving
a single Zcash-indexer domain, so one root `CONTEXT.md` + `docs/adr/` covers it.
If the project later splits into genuinely separate domains, switch to a
multi-context layout (a root `CONTEXT-MAP.md` pointing at per-area `CONTEXT.md`
files) and update this file.

**ADR home.** Numbered architecture decision records live in **`docs/adr/`** (e.g.
`0001-zcashd-support-feature-gate.md`). `docs/decision_records/` is a *separate*
area for process notes (the release process) — it is **not** the ADR surface;
don't read or write ADRs there.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a
hypothesis, a test name), use the term as defined in `CONTEXT.md`. Don't drift to
synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal — either you're
inventing language the project doesn't use (reconsider) or there's a real gap
(note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than
silently overriding:

> _Contradicts ADR-0001 (zcashd-support feature gate) — but worth reopening
> because…_
