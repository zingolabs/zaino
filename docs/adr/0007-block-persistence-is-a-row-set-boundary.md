# Block persistence is a row-set boundary; the domain block is not a storage value

## Status

accepted

## Context and decision

`IndexedBlock` is zaino's domain block aggregate: the typed, tx-major,
chain-positioned block that the non-finalised state holds in memory and
that both boundary projections derive from (`to_compact_block()` toward
the wire, the finalised writer toward disk). For its entire history the
type also *claimed* to be a storage value: it carried a wholesale
`ZainoVersionedSerde` impl defining a monolithic single-value block
format.

That claim was never true in production. The archaeology (July 2026):
the encoding was written in May 2025, two days after the type itself
(then named `ChainBlock`), with zero consumers, in anticipation of the
coming finalised state. When the finalised state actually arrived it
made a different choice both times — v0 stored wire `CompactBlock`s
derived via `to_compact_block()`, and v1 stores decomposed **pool-major
rows** (`BlockHeaderData`, `TxidList`, per-pool tx lists,
`CommitmentTreeData`), rebuilding blocks by transposition. The wholesale
format's only callers were ever test-vector fixtures, deleted in autumn
2025. The impl survived on classification alone: when the October 2025
types refactor dropped it, commit `ded77d15` restored it as "missing"
on the reasoning that `IndexedBlock` "is a database-serializable type."
No bytes in the format ever existed in any production database.

We decide:

1. **The storage form of a block is the set of v1 rows** — pool-major,
   independently versioned via `ZainoVersionedSerde`, and readable in
   pool-filtered subsets. There is no single-value block format.
2. **`IndexedBlock` is not a storage value and does not implement
   `ZainoVersionedSerde`.** The dead impls (`IndexedBlock`, and
   `CompactTxData`, whose only caller was `IndexedBlock::decode_v1`)
   are deleted, not maintained. Do not "fix" this by restoring them —
   that is precisely the `ded77d15` failure mode this ADR exists to
   prevent. A block reaches disk only by decomposition into rows and
   returns only by reassembly from them.
3. **The block ↔ row-set correspondence gets one type-level home**: a
   `Persistent*`-doctrine twin (working name `PersistentBlock`) holding
   the row types as fields, with `from_business(&IndexedBlock)` owning
   the tx-major → pool-major scatter and `into_business()` owning the
   reverse transposition. The v1 writer (including the schema-compat
   downgrade writer, which scatters the same block into old-layout
   rows) and the v1 reader both delegate to it, so the two directions
   are halves of one impl with a round-trip law, instead of two distant
   procedural code paths kept inverse by discipline. `PersistentBlock`
   itself is **not** a stored value and does not implement
   `ZainoVersionedSerde`; each row still stores independently.

## Considered options

- **Keep the wholesale impl as a second schema** (status quo):
  rejected — a complete storage format with no bytes behind it,
  maintained through two renames and multiple refactors, repeatedly
  mistaken for load-bearing.
- **Adopt the wholesale format** (store blocks as single values):
  rejected by the schema's actual requirements — pool-filtered serving
  reads row subsets, and the transposed grain is the point of the v1
  layout.
- **Keep the correspondence procedural** (writer and reader each state
  the decomposition): rejected — a field added to one side and
  forgotten on the other is a runtime bug; in the twin it is a compile
  error.

## Consequences

- Every role now has exactly one home: proto types for the wire,
  `IndexedBlock` for the domain, row types (+ `PersistentBlock` as
  their aggregate correspondence) for storage — with named conversions
  as the only crossings, per the boundary-conversion doctrine in
  CLAUDE.md.
- The deletion needs no data migration: there are no bytes in the
  removed format anywhere.
- The wire ↔ domain boundary is already schema-first (`.proto` is the
  single declaration; prost generates the foreign twin;
  `to_wire`/`try_from_wire` carry validation) and needs no analogous
  change.
