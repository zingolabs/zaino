# Crate boundaries follow an independent-variation criterion

## Status

proposed

Number provisional: `0011` is reserved for the chain-head-subsystem ADR carried by
#1440 (it currently numbers itself `0010`, which collides with the mempool ADR
already on `dev`, so it renumbers to `0011` on rebase). Assign the final number at
merge.

This ADR does not introduce a new rule so much as *name* one that ADR-0003,
ADR-0007, ADR-0008, and ADR-0010 already apply. It exists so the rule can be
cited and applied consistently as the workspace decomposition continues.

## Context

The workspace is being decomposed from the `zaino-state` / `zaino-fetch` monolith
into many small crates (#1402 modular sync engine; #1418 driving-port scaffold;
#1440 chain-head isolation; the mempool split in ADR-0010). A question recurs at
every step: **when should a boundary become its own crate?**

A layout proposal under discussion answers it uniformly — a `<domain>` +
`<domain>-service` crate *pair* for every subsystem. But two in-flight PRs drew
*different* boundaries around the *same* non-finalised subsystem: #1440 split it
into a port crate (`zaino-chain-head`) plus a service crate
(`zaino-chain-head-service`); #1418 kept it as a single crate (`zaino-nfs`). Both
are defensible. Without a stated rule, this kind of disagreement has no principled
resolution, and "more separation is better" trends toward unbounded crate
proliferation.

Prior ADRs already decide from the same rule without naming it:

- ADR-0003 justified a two-crate test split by "zero cross-references, splitting
  duplicates no code, disjoint feature tables."
- ADR-0007 gave each role (wire / domain / storage) exactly one home.
- ADR-0008 / ADR-0010 split mempool and chain-head into a ports crate plus a
  runtime crate.

## Decision

**1. Classify every crate by one of four roles.**

- **vocabulary** — shared data types, no interfaces, near-zero deps
  (`zaino-primitives`).
- **driven-port** — an interface a component *requires* and calls outward through;
  filled by an adapter below (`zaino-source`, `zaino-persistence`).
- **driving-port** — an interface a component *offers* and others call into; the
  crate's identity (the read-capability traits).
- **conformer** — one concrete implementation of a port; where heavy dependencies
  live (`*-service`, `zaino-source-zebra-*`, `zaino-backend-lmdb`, mocks).

Legal dependency edges follow from the roles: everyone may depend on vocabulary; a
conformer depends on its port; a higher composer depends on lower *ports*, never
lower conformers (except at the process root `zainod`).

**2. A crate is the unit of independent variation.** Keep conceptual boundaries
(a trait, a private type, a mock) everywhere — they are free. Promote a boundary to
a *separate crate* only when something genuinely varies across it for a real
stakeholder. Split when **at least one** axis holds:

| Axis | Split when… |
|---|---|
| Dependencies differ | one side pulls a heavy dep (LMDB, networking, Zebra) a consumer of the other side wants to avoid compiling |
| ≥2 implementations | the interface has, or soon will have, two backings needing a shared contract — *including a single impl written as a swappable trait* |
| Outside consumer | someone binds it out-of-tree, versioned/published (a test/mock always does; sometimes a foreign user does) |
| Release cadence | it must ship or version on its own schedule |

If no axis holds, the boundary stays a **module** inside a larger crate. The
decision is not permanent: a fused crate legitimately splits later, the moment a
second implementation or a heavy dependency arrives.

**3. Reject uniform `<domain>` + `<domain>-service` pairing as a blanket rule.**
The pair is the right instinct at the *conceptual* level (a trait + mock + contract
belongs to every subsystem) but wrong as an automatic *crate* split. Decide per
boundary by the axes above.

**4. Port well-formedness is a dependency-graph property.** A port crate satisfies
`deps(port) ⊆ vocabulary tier` — it never lists a conformer/infra dependency. This
makes a type leak (e.g. a backend type in a port signature) a *compile error*, not
a review-time catch, and it is checkable mechanically over the crate graph in CI
(the same posture as the existing `makers lint-boundary-conversions`).

## Considered alternatives

- **Uniform `<domain>`/`-service` pairing everywhere.** Rejected: it asserts crate
  boundaries the compiler has not ratified from a second side, and it cannot
  explain why #1418 and #1440 reasonably chose *different* boundaries for the same
  subsystem. It also multiplies manifest and release surface with no offsetting
  variation.
- **Keep large multi-concern crates (status quo `zaino-state`).** Rejected: the
  crate is the only real isolation boundary in Rust, so a monolith forces
  unrelated concerns to collide (the documented `zaino-state` god-module churn).
- **Split on dependencies only.** Rejected: it under-splits. #1440's
  `zaino-chain-head` / `-service` carries *no* heavy-dependency divergence yet is
  correctly split — on the *swappable-trait* axis (`ChainHeadSnapshot` is a trait)
  and the *test-consumer* axis (a substantial mock-backed suite). A deps-only rule
  would wrongly call that split needless.

## Consequences

- The `zaino-chain-head` / `-service` split (#1440) is **earned** — on the
  substitutability and test axes, not dependencies. `zaino-nfs` staying a single
  crate (#1418) is also **fine at its current maturity** (one impl, `im`-only);
  it splits when a second backing or a heavy dep lands. Both are correct under the
  rule.
- The mempool split (ADR-0010) and the source/persistence port-vs-adapter splits
  are instances of this same rule, now stated once.
- **Interaction with versioning policy:** the *outside-consumer* axis admits
  deliberately-reusable driven-port crates (e.g. a third party building on
  `zaino-source` to talk to a validator). If any such crate is treated as
  public-facing, the "version only the binary and public library" stance needs a
  small scope note. Flagged, not decided here.
- Reconciling #1418 and #1440 uses this criterion rather than an appeal to
  uniformity.
