# Decision records

Subsystem-scoped decision records. Each subdirectory holds the records and
specifications of one subsystem (`release/`: the release pipeline).

## Relationship to `docs/adr/`

`docs/adr/` is the repo-wide, numbered ledger. A decision that reaches beyond
its subsystem graduates to a numbered ADR. Both trees follow the same ledger
rule.

## The ledger rule

Records are append-only. Supersede, never delete or rewrite: a superseded
record stays in the tree with a status header naming its successor
(see `release/periodic.md`). A record documents the decision as made — it does
not track the code, so "the code changed" is never a reason to remove one.

## Records vs specifications

A record states one decision: context, decision, consequences. Keep it short.
A specification (e.g. `release/pipeline.md`) states the full governed
behaviour and may be long; its Status section names what it supersedes and
its revision history carries the decision trail.
