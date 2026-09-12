# Architecture Decision Records

The repo's single decision ledger. "Architecture" reads broadly: any decision
that shapes how this repo is built, tested, or released belongs here —
system structure, process, and infrastructure alike.

## The ledger rule

Records are append-only. Supersede, never delete or rewrite: a reversed
decision gets a new record marked "supersedes N", and the old record gets a
superseded-status header naming its successor (see ADR-0015). A record
documents the decision as made — it does not track the code, so "the code
changed" is never a reason to remove one.

## Records vs specifications

A record states one decision: context, decision, consequences. Keep it short
(target ~40 lines). Full governed behaviour belongs in a specification
elsewhere in `docs/` (e.g. `docs/release/pipeline.md`), revised in place and
referenced from its record. When a decision only exists inside a long
document, extract it into a record here.

## Numbering

Take the next free number when proposing. Concurrent proposals may collide;
renumber at merge, not before. A gap in the sequence means a proposal was
abandoned or renumbered, nothing more.
