# Parallel-buildable finalised spend index (POC)

## Status

proposed

Refines the [#1326](https://github.com/zingolabs/zaino/issues/1326) finalised-state
reshaping conversation ("Proposal 1"); tracked as the per-index proposal in
[#1328](https://github.com/zingolabs/zaino/issues/1328). Implementation lands in
a separate, discrete PR after this ADR is committed.

## Context and decision

Zaino's finalised state is today a block-oriented monolith: one per-block write
path (`write_block_with_options`) fans out across ~12 tables in a single
transaction. #1326 explores reshaping that schema. This ADR records the first
concrete step, built from first principles: a single, **standalone**,
**parallel-buildable** finalised index, as a proof of concept that the sync
procedure can be decomposed into isolated per-index builds and debugged against
the simplest useful case.

The chosen index answers one query:

> Given a transparent outpoint, return the txid of the transaction that
> **consumed** it, or `None` if it is unspent.

```
transparent_outpoint  ->  spending_txid
```

Terminology (recorded in `chain_index/CONTEXT.md`): an outpoint already names its
**creating** transaction (its first 32-byte field). The transaction that
*contains* an outpoint — as the prevout field of one of its **inputs** — is its
**spending** transaction. This index maps to the spending txid, never the
creating txid.

**Why build it rather than proxy zebra.** zebra already answers this via
`ReadRequest::SpendingTransactionId(Spend::OutPoint(..))`, but only behind its
`#[cfg(feature = "indexer")]`. Building it in zaino gives (a) independence from a
non-default zebra feature, (b) serving scale (zaino owns the read path), and
(c) a pilot for the parallel-sync architecture. Decisively: zebra's `indexer`
service is slated to be **deprecated and removed** once this is stable in zaino,
so zaino becomes the sole provider and this index becomes *required*, not merely
offered. zebra's `SpendingTransactionId` is therefore a **temporary correctness
oracle** — golden vectors must be captured before the feature is removed.

## The index shape

- **Key:** the 36-byte outpoint (`prev_txid[32] ‖ LE(u32) prev_index`). We key by
  the outpoint because that is what the query hands us — keying direction *is*
  the design.
- **Value:** the bare 32-byte spending txid. Not a `TxLocation`: a location would
  be resolvable to a txid only via the `txids`/`txid_location` tables, which would
  make this index depend on another at build and serve time and destroy its
  isolation. Storing the txid directly keeps the index a pure function of the
  block stream.
- **Absence ⇒ unspent.**

## The read-free extractor and the three-role split

The monolith decomposes into three roles, and the "no external lookup" property
is scoped to exactly one of them:

| Role | Purity | Holds | Job |
|---|---|---|---|
| Block source | impure | `BlockchainSource` (zebra) | produce `IndexedBlock`s |
| **Extractor** | **statically read-free** | *only `&[IndexedBlock]`* | `&[IndexedBlock] → Vec<(outpoint, spending_txid)>` |
| Collator | impure, single-writer | the LMDB write txn | sorted-merge → `MDB_APPEND` → store |

The extractor walks each transaction's inputs and emits `(prevout_outpoint,
this_txid)`, skipping inputs whose prevout is **null** (`is_null_prevout()`).
The only such input is the coinbase transaction's, which mints the block
subsidy and references no prior output — emitting a record for it would key a
bogus entry on the all-zeros/`u32::MAX` sentinel. Spends *of* coinbase outputs
are ordinary outpoint spends and **are** indexed; only the coinbase *input* is
skipped. (Consensus forbids null prevouts in non-coinbase transactions, so the
null-prevout test identifies exactly the coinbase input without needing to
recognise a coinbase transaction structurally.) Its read-freedom is enforced by its
**signature**: a free function handed only `&[IndexedBlock]`, with no `&self`, no
`Database`, no source handle in scope — so a DB or validator lookup is
unrepresentable, not merely discouraged. zaino's compact representation already
carries the data it needs: `IndexedBlock → CompactTxData` exposes `txid()` (the
value) and `transparent().inputs()` (the prevout outpoints), so no fuller block
form is required. Precise scope: the *extractor* does zero reads; the *orchestrator*
legitimately reads to source blocks.

## Build model (POC)

- **Clean-sync, streaming from zebra, re-streaming from genesis.** No backfill from
  on-disk block tables, no resume watermark, no migration, no block-table writes.
  The only persisted artifact is the index. A crash simply reruns from genesis.
- **Finalised-only boundary.** The orchestrator only feeds the extractor block
  ranges below the seam (`finalised_tip = best_height − non_finalized_depth`), so
  the index is reorg-immune by construction.
- **Disjoint keys ⇒ trivial collation.** Every outpoint is spent at most once
  chain-wide, so per-batch deltas have globally disjoint keys; collation is a pure
  sorted merge with zero cross-batch reconciliation (unlike value/script-bearing
  indexes, which would need a shared `live_utxo` primary). Build: each parallel
  worker owns a contiguous height sub-range, appends to its own `Vec` (the fastest
  possible writer), sorts its run; the runs are k-way merged into a single LMDB
  `MDB_APPEND` load.

## Independent, compile-time-single loop

The build runs as an **independent loop** — its own zebra stream and LMDB index,
decoupled from `write_blocks_to_height` and the monolith.

It is constrained so that **only one loop can run at a time**, enforced as far as
the type system allows:

- The loop is a **move-only (`!Clone`) handle** whose `run` **consumes `self`**.
  Starting it (`tokio::spawn(async move { sync.run().await })`) moves the unique
  handle into the task, leaving none behind; a second concurrent loop is rejected
  by the move checker. `self`-by-value (one-shot; reconstruct to rerun) is chosen
  over `&mut self` because it is strictly stronger and matches genesis re-streaming.
- The constructor is **private** (module privacy is a compile-time guarantee that
  external code cannot mint one), and the owning component holds exactly one
  instance.

Honest seam: the type system forbids *duplicating* the handle and *concurrent*
`run`s; it cannot forbid the owner calling the private constructor twice. That
residual is closed by discipline (a single `new()` call in the owner), not by a
runtime guard (`OnceLock`/atomic) — deliberately, since a runtime guard is the
mechanism this constraint exists to avoid.

## Storage and integrity

Per-record checksums are **dropped** for this index (≈ 78–86 B/entry; ≈ 5 GB
saved at mainnet scale). Tamper-resistance — Zaino must return *corrupt chain
data* rather than serve a wrong txid — is preserved at the **table level** by an
order-independent XOR-of-BLAKE2b commitment over each `(outpoint ‖ spending_txid)`
entry, the same multiset construction the txout-set accumulator already uses.
Being self-inverse and order-independent, each parallel batch computes its own
partial commitment and they combine for free at collation; it is verified at
startup. The trade-off — loss of corruption *localization* — is accepted, since
the index is deterministically rebuildable. This is the table-oriented direction
raised in #1326's validation-model note.

## Module organization

The struct, its impl, the run loop, extractor, and collator live together in a
**single self-contained module-and-file**, gated by a new default-off Cargo
feature `spend_index_experimental` (grouped under `experimental_features`,
mirroring `transparent_address_history_experimental`). Visibility is minimized to
`pub(super)` toward the module's sole owner. The exact placement (which parent
module makes `pub(super)` sufficient) is an implementation detail resolved in the
PR.

## Sizing (mainnet, height ≈ 3,395,127, 2026-06-29)

The entry count is the number of transparent spends ever:

| Source | Method | Ostensible spends |
|---|---|---|
| Blockchair `outputs?q=is_spent(true)` | direct count of spent transparent outputs | 162,381,790 |
| Blockchair (identity) | outputs created − UTXO set = 189,595,212 − 27,999,653 | 161,595,559 |
| 3xpl | 372.2M indexed rows − 189.6M outputs (incl. shielded ⇒ inflated) | ≈182.6M |

⇒ ≈162M entries, **~13–15.5 GB**, growing ~linearly with chain history.

## Alternatives considered

- **Proxy zebra's `indexer`** — rejected: it is being deprecated and couples zaino
  to a non-default zebra build.
- **Store `TxLocation` instead of the txid** (today's `spent` table) — rejected:
  reintroduces a cross-table dependency and breaks isolation.
- **Two-table decomposition** (`outpoint → creating_txid` + `spending_txid →
  spent_outpoints`) — rejected: the first is the identity projection of the
  outpoint's own field, the second is keyed in the wrong direction; jointly they
  cannot serve the query as a point lookup.
- **Per-record checksums** — rejected for this index in favour of a table-level
  XOR commitment (localization deliberately sacrificed).
- **Backfill from on-disk block tables** — dropped for the POC; clean-sync genesis
  re-stream only.
- **Runtime singleton guard** (`OnceLock`/atomic "am I running") — rejected in
  favour of the compile-time move-only handle.
- **Immutable sorted-segment (SSTable) durable form** — deferred: it is the leading
  v2 candidate (smaller, write-once-sequential, a natural fit for the append-only
  finalised invariant; cf. zebra's RocksDB LSM), but too large for the pilot. LMDB
  (`MDB_APPEND`) is used now to reuse the existing engine. Logged to #1326.

## Consequences

- The POC is out of default builds until proven; no production impact while gated.
- The first build re-streams genesis..tip from zebra rather than reading local
  blocks — a one-time fetch cost bought in exchange for total decoupling. No resume
  in the POC: a crash reruns from genesis.
- The *served* answer is incomplete for reorg-window spends until the deferred
  serve-time seam union (`finalised index ∪ NFS spends`) lands; the finalised index
  alone returns `None` for an outpoint spent only within the non-finalized depth.
- The zebra oracle is temporary; golden vectors must be captured before zebra
  removes `feature = "indexer"`, after which zaino is the sole provider.
- This index's integrity is validated table-level, diverging from the per-record
  checksum convention of the other v1 tables — intentional, and aligned with the
  validation-model reconsideration in #1326.
