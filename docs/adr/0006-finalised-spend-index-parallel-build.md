# A standalone, parallel-buildable finalised spend index

## Status

accepted (pilot; refines the umbrella schema discussion in issue #1326,
scoped by issue #1328, implemented in PR #1330)

Note: issues #1328/#1326 reserved the path
`docs/adr/0002-finalised-spend-index-parallel-build.md` for this record;
that number was taken by the live-tests ADR before this file landed, so
the record lives here as 0006.

## Context

Serving "which transaction spent this transparent outpoint?" requires an
index mapping `transparent_outpoint → spending_txid`. zebra already
answers this via `ReadRequest::SpendingTransactionId`, but only behind
its non-default `indexer` feature, and that service is slated for
removal once zaino provides the index — so zaino becomes the sole
provider and the index becomes required, not merely offered. zebra's
answer is therefore a temporary correctness oracle: zaino's index is
diffed against it, and golden vectors must be captured before the
feature is removed.

On the serving side, zaino already answers the query:
`ChainIndex::get_outpoint_spenders` (PR #1167) takes a batch of
outpoints and a `ChainScope` — `FullChain` scans the non-finalised best
chain first, then falls back to the finalised state; `Finalised` is the
reorg-stable subset. Its finalised leg reads the monolith's `spent`
table (`outpoint → TxLocation`) and resolves each deduped `TxLocation`
to a txid through the transaction-location tables. That method is the
consumer this index ultimately backs.

Independently, the finalised-state build path is a monolith: one
`write_block_with_options` writes all tables per block in a single
transaction, so no index can be built, rebuilt, or optimised in
isolation. The spend index is the simplest useful pilot for breaking
that coupling: its keys are globally disjoint (an outpoint is spent at
most once chain-wide), so per-batch results combine by pure sorted merge
with zero cross-batch reconciliation — unlike value- or script-bearing
indexes, which need a shared live-UTXO primary.

Terminology (see `packages/zaino-state/src/chain_index/CONTEXT.md`): an
outpoint's first field already names its **creating** transaction; the
transaction that lists the outpoint as an input's prevout is its
**spending** transaction. This index maps to the spending txid only.

## Decision

Build the spend index in zaino as a standalone pilot, gated behind the
default-off Cargo feature `outp_to_spend_index` (a member of
`experimental_features`), decomposed into three roles so that
read-freedom is scoped to exactly one of them:

- **Block source** (impure): a `BlockchainSource` producing zebra
  blocks. Bound at compile time to zebra's StateService or the
  test mockchain via the `SpendIndexSource` trait — the JSON-RPC/zcashd
  `FetchService` does not implement it, so "never FetchService" is a
  compile-time fact. The fetch is **roots-free**: one `get_block` per
  height, skipping the monolith ingestion's second sequential await
  (`get_commitment_tree_roots`) and its compact conversion of shielded
  data — the spend index needs neither, and PR #1241 measured that
  two-await-per-block pattern collapsing to ~1 blk/s in the sandblast
  band.
- **Extractor** (statically read-free): a free function handed only
  fetched block data — no `&self`, no database, no validator handle in
  scope — so a previous-output lookup is unrepresentable, not merely
  discouraged. Each transparent input yields
  `(prevout_outpoint, containing_txid)`. Null-prevout inputs (only the
  coinbase input) are skipped; spends *of* coinbase outputs are ordinary
  spends and are indexed. It exists in two forms — over raw zebra blocks
  (the build path) and over zaino's compact form (the original, kept as
  the test oracle) — and the sync-loop tests assert the two agree over
  the same chains.
- **Collator/store** (impure, single-writer): encode, byte-sort, reject
  duplicate keys as corrupt input, and bulk-load with `MDB_APPEND` into
  the index's **own** LMDB environment (database
  `outp_to_spend_index_1_0_0`, no `WRITE_MAP`, sited as a sibling
  directory of the chain-index database), a sequential B-tree fill.

Schema: key = the 36-byte encoded outpoint (`txid[32] ‖ LE(u32) index`)
because that is what the query hands us; value = the bare 32-byte
spending txid; absence ⇒ unspent **within the index's built range**
`[index floor, finalised tip]` — below its floor the index asserts
nothing. The value is
deliberately **not** a `TxLocation`: that would reintroduce a dependency
on the `txids`/`txid_location` tables and break the index's isolation.

Build model (pilot): a clean sync that re-streams blocks from the
source over `[start_height, finalised_tip]`, where
`finalised_tip = best_height − non-finalized depth` — feeding the
extractor only finalised blocks makes the build reorg-immune by
construction. No resume watermark, no backfill from on-disk block
tables, no migration; a crash reruns from the start height. The
streaming stage fans out across worker tasks pulling fixed-size height
chunks from a shared queue (chunk-pulling self-balances the block-weight
skew across the chain); collation stays one global sort feeding one
append pass, and workers never touch the store — the single-writer
discipline whose violation PR #1275 diagnosed as LMDB corruption.
There is one code path, not two: `workers = 1` *is* the serial
baseline, so serial-vs-parallel benchmarks vary only the fan-out, and
the build reports per-stage timings (stream/extract, collate, load)
plus its worker count to make every run a self-describing measurement.

The start height — the **index floor** — is a first-class, configurable
property of the index that survives into production, not pilot
scaffolding: genesis buys full coverage; a later floor (e.g. a
network-upgrade activation) buys a cheaper build at the cost of
range-scoped answers. Serving discipline: a deployment that serves
spend queries must run a genesis floor, enforced by config validation,
so a floor-truncated answer never reaches a client; non-genesis floors
are for deployments that build without serving (pilots, tests,
experiments). The shipped configuration defaults to genesis and carries
the Sapling-activation floor only as a commented-out option in the
config file. Because absence is only meaningful relative to the built
range, a serving layer must know the index's floor, so the floor must
ultimately be persisted with the index (deferred; the pilot hardcodes
it). The pilot enters through one `spawn_build` call with the floor
pinned to Sapling activation.

Single-loop enforcement is pushed as far into the type system as it
goes: the sync handle is move-only (`!Clone`) with a private constructor
and a `self`-consuming `run`, so duplicating the handle or running two
loops concurrently is rejected by the move checker. The residual — the
owner minting two handles — is closed by the owning `ChainIndex` making
exactly one `spawn_build` call, deliberately not by a runtime guard
(`OnceLock`/atomic). The owner holds the returned `JoinHandle` and
aborts the build on shutdown/drop.

## Considered options

- **Keep serving from the monolith's `spent` (`outpoint → TxLocation`)
  table alone** — what `get_outpoint_spenders`' finalised leg does
  today. Rejected as the end state: that table is written only by the
  monolithic per-block write path, so it cannot be built, rebuilt, or
  optimised in isolation, and its `TxLocation` value forces a second
  resolution hop through the transaction-location tables — exactly the
  coupling the bare-txid value avoids. It remains the serving path
  until this index replaces the finalised leg.
- **Unconditional genesis start (no index floor).** Rejected: it would
  make "absence ⇒ unspent" hold outright, but forces every deployment
  to carry the full deep history whether or not it serves it. The
  configurable floor keeps the build/storage cost proportional to what
  a deployment actually answers for; the cost — absence is range-scoped
  — is carried explicitly by the serving layer.
- **Serving with a non-genesis floor** — either falling back to the
  monolith's `spent` table below the floor, or letting
  `get_outpoint_spenders`' "`None` = unspent or unknown" contract
  absorb the weaker answer. Both rejected: the fallback keeps the
  monolith coupling alive indefinitely, and contract absorption
  silently serves worse answers than the monolith's `spent` table does
  today. Config validation (serving ⇒ genesis floor) prevents both.
- **Proxy zebra's `indexer` service instead of building.** Rejected:
  ties a served capability to a non-default zebra feature that is
  planned for removal, and forfeits both serving scale and the pilot
  value for the parallel-sync architecture.
- **Value = `TxLocation` instead of bare txid.** Rejected: couples the
  new index to the monolith's transaction-location tables, defeating
  the isolation this pilot exists to prove.
- **Runtime single-instance guard.** Rejected in favour of the
  move-only consuming handle plus a single constructor call site;
  a `OnceLock`/atomic guard would paper over ownership the type system
  can express.
- **Backfill from zaino's on-disk block tables.** Dropped: re-streaming
  from the validator keeps the build independent of every other table
  and is the model production resume will refine, not replace.
- **Immutable sorted segment (SSTable-like flat file) as the store.**
  Deferred to the v2 discussion in #1326: a better long-term fit for an
  append-only finalised index than LMDB's update-in-place B-tree, but
  the pilot reuses the engine already in the tree.

## Consequences

- ≈162M entries at mainnet height ≈3.4M ⇒ roughly 13–15 GB on disk,
  growing linearly with chain history; the pilot's LMDB map size is a
  32 GB lazy upper bound, not a preallocation.
- The build holds all extracted spends in memory for one global sort
  and a single `MDB_APPEND` transaction; `bulk_load` is one-shot (a
  second call would need every key to exceed the first call's maximum).
  This is deliberate, not a pilot shortcut: the pre-materialized,
  globally sorted single append pass is the speed-of-light baseline
  for the sync benchmark, and at genesis floor on mainnet it peaks
  around 30 GB — within a reasonably resourced build machine. A
  batched variant (per-worker sorted runs, k-way merge feeding the
  appender — licensed by the disjoint-key property) is a contingency
  adopted only if measurement shows it beating the one-shot pass or
  memory genuinely binding, not a foregone production requirement.
- The serve-time union with the non-finalised state already exists as
  `get_outpoint_spenders`' `FullChain` scope (PR #1167); the pilot index
  is built but not yet served. The deferred serving step is swapping
  that method's finalised leg from the monolith's `spent` table to this
  index — dropping the `TxLocation → txid` resolution hop — with no
  change to the method's contract (it already returns bare txids).
  Queried directly, the pilot index alone returns `None` for an
  outpoint spent within the non-finalized depth, and for anything
  spent below its floor (pilot: Sapling activation).
- With a floor above genesis, `None` from the index means "unspent or
  spent below the floor". Config validation keeps that answer away from
  clients — serving requires a genesis floor — so the swap needs no
  fallback to the monolith's `spent` table and the isolation goal
  stands. The floor must still be persisted in the index's environment
  so builders, restarts, and servers agree on the built range; it is
  the value the serving-side validation checks.
- `get_outpoint_spenders(Finalised)` over the monolith's `spent` table
  is a second, in-tree parity oracle for this index, alongside zebra's
  `SpendingTransactionId` — same chain, same process, no non-default
  zebra feature required.
- Table-level integrity (the order-independent XOR-of-BLAKE2b
  commitment over entries sketched in #1328) is deferred; the pilot
  index has no checksum and relies on being deterministically
  rebuildable.
- The pilot's acceptance bar is a mainnet run of the one-shot build on
  a well-resourced machine, producing three artifacts together: the
  sync-time benchmark numbers (the pilot's reason to exist), a parity
  diff against zebra's `SpendingTransactionId`, and golden vectors —
  which must in any case be captured while zebra still ships the
  `indexer` feature. A synthetic-chain parity test runs first, to prove
  the oracle plumbing before spending a mainnet stream on it.
- Generalising the extractor/collator decomposition to value- and
  script-bearing indexes (txout-set accumulator, address history) —
  which need a shared live-UTXO primary — remains with #1326.
