# Design Explorations (thought experiments)

This directory holds **design explorations** — speculative thinking recorded for
posterity. An exploration is explicitly **not a decision**: it binds nothing,
schedules nothing, and is plausibly never revisited, much less implemented.

It is a deliberately separate category from `docs/adr/`:

| | `docs/adr/` | `docs/explorations/` |
|---|---|---|
| Purpose | the **decision log** — commitments | recorded **speculation** |
| A reader may assume | the project is doing / has decided this | nothing; it may never happen |
| Filename | `NNNN-kebab-title.md` | `EXP-NNNN-kebab-title.md` |
| `Status` values | `proposed` → `accepted` / `rejected` / `deprecated` / `superseded` | `Exploratory` / `Dormant` / `Abandoned` / `Promoted → ADR-NNNN` |

**Why separate, not just an ADR status.** An ADR log readers cannot trust is
worse than no log. Filing a thought experiment as an ADR — even `proposed` or
`rejected` — pollutes the record of actual commitments and tempts a reader (or an
agent) to treat a muse as a plan and build around it. Keeping explorations in
their own space, behind an unmistakable `EXP-` prefix, keeps `docs/adr/` a clean
record of what was actually decided.

## Rules

1. **Mandatory banner.** Every exploration opens (right after its `# EXP-NNNN: …`
   title) with the design-exploration banner stating it is not a decision, not
   scheduled, and may never be implemented.
2. **Status field** from the enum above. Default `Exploratory`. Use `Abandoned`
   (not deletion) when you decide against pursuing it, so it is not re-litigated.
3. **Independent numbering.** `EXP-NNNN` numbers are their own sequence; they do
   not consume ADR numbers.
4. **Reference hygiene.** Other docs cite an exploration only *as speculative*
   (e.g. "the speculative rebuild-and-cutover model, EXP-0001"). An ADR may
   reference one only conditionally ("if EXP-0001 is ever pursued…"). Never
   present an exploration as established.
5. **Promotion.** If an exploration graduates into a real decision, create a
   *new* ADR in `docs/adr/` and set the exploration's status to
   `Promoted → ADR-NNNN`. The exploration file stays as the origin record.

## Index

- [EXP-0001 — Rebuild-and-cutover finalised-state schema upgrades](EXP-0001-rebuild-and-cutover-schema-upgrades.md)
  — `Exploratory`. Speculative alternative to stepwise finalised-state migration.
