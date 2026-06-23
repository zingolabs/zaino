# Zaino

Zaino is a Zcash indexer that serves wallet/RPC traffic from its own validated
view of the chain, backed by a validator (zebra, or — being deprecated — zcashd).

## Language

### Chain state

**Finalization ceiling**:
The height `chain_tip − NON_FINALIZED_DEPTH`. At or below it a block is
*finalized* — immutable, reorg-safe to fetch from the validator by height. Above
it is the reorg-mutable non-finalized window. The value tracks the chain tip, so
it can move *backwards* after a chain-shortening reorg (see zaino#1128); it is
not monotonic.
_Avoid_: finalized height floor, NFS floor, anchor height — for the boundary
*value*. (The code function is `finalization_ceiling`, matching the
`reify_NFS_when_FS_synced` draft.)

**Non-finalised state (NFS)**:
Zaino's validator-sourced view of the reorg-mutable window `[ceiling, tip]`. The
NFS *leads* the finalised DB and never waits for it to catch up.
_Avoid_: non-finalized cache, mempool (unrelated).

**NFS anchor (seam block)**:
The block at the finalization ceiling that roots the non-finalized window. It is
served from the finalised DB when that DB has reached the ceiling, otherwise from
the validator directly. The anchor is defined by the ceiling height alone — *not*
by wherever the finalised DB tip currently sits.
_Avoid_: root block, genesis seed.

**Finalised state / finalised tip**:
The durable on-disk index of finalized blocks; the *finalised tip* is its highest
stored height (`db_height`). It lags the finalization ceiling during background
catch-up and equals it in steady state — it never determines the NFS anchor.
_Avoid_: finalized database height (when referring to the tip value).

**Provisional**:
The condition where the finalised DB has not yet caught up to the NFS, so a height
in `[finalised_tip, ceiling]` is served via the validator passthrough rather than
from the durable index (see zaino#1096).

### Finalised-state tables

The finalised state is a set of LMDB named databases. Three role words keep them
distinct; do not call them all "indices".

**Table (logical database)**:
The generic unit — one LMDB named database, a single key→value B+tree. The
neutral word for any one of them when role is irrelevant. The schema file calls
these "logical databases"; #858 calls them "indexes" loosely.
_Avoid_: "index" as the generic word — it has a narrower meaning below.

**Primary store**:
A table holding a block's own content, keyed by height — the chain data itself,
not a lookup accelerator. These are what the validator does not serve in compact
form, so they are the indexer's reason to exist (e.g. the compact-block and
header/txid tables).
_Avoid_: calling a primary store an "index".

**Index**:
The narrow sense — a table that is a *reverse or secondary lookup*, derived from
primary data purely to accelerate access by a non-height key (hash→height,
txid→location, outpoint→spender, address→events). Dropping an index loses speed,
not data: the same answer can be recomputed or fetched via validator passthrough.
_Avoid_: using "index" for primary stores or singletons.

**Singleton**:
A table holding a single fixed-key record — config or a whole-chain aggregate,
not keyed by block (the metadata record; the txout-set accumulator).
_Avoid_: calling a singleton an "index".

**Ephemeral mode**:
A finalised-state mode (`ChainIndexConfig { ephemeral: true }`) in which **no
persistent finalised-state DB is opened** — finalised reads are served live from
the backing validator instead of from built tables. zallet runs Zaino this way
in-process (it embeds `NodeBackedChainIndex`), so zallet builds none of the 12
tables. Effectively the always-on form of the `FeatureUnavailable` → passthrough
model (zaino#861).
_Avoid_: assuming an embedded Zaino implies a Zaino DB on disk — ephemeral mode
has none.

**Required set vs offered floor**:
Two different sets, and the difference is the whole point of the zallet-indices
analysis (see `docs/notes/zallet-required-indices.md`).
- *Required set* = tables zallet genuinely cannot get elsewhere. With zallet
  sourcing all it can from the validator, and running Zaino ephemerally, this is
  **∅ (empty)** today. (verified against zcash/wallet PR #486)
- *Offered floor (DbV2)* = the smallest coherent persistent skeleton a
  small-footprint major would store *if* ephemeral mode were ever disabled —
  justified by single-endpoint / offload-validator, not by requirement. Per
  zaino#860: `headers`, `txids`, `heights`, `metadata`; capabilities
  `READ_CORE`, `WRITE_CORE`, `BLOCK_CORE_EXT` (note `BLOCK_CORE_EXT` also implies
  the `txid_location` index — see the doc).
_Avoid_: calling the offered floor "required", or conflating either with DbV1
(the full-footprint superset).

### Feature gating of DB rows

A Cargo feature can decide whether a given category of row is written to the
finalised DB. Two opposite conventions exist, and they must not be conflated:

**Additive gate**:
A feature whose default (absent) state writes *no* rows of the category, and
*enabling* it turns the writes on. The category is opt-in experimental surface.
The first such gate is `transparent_address_history_experimental` (writes the
address-history rows). The naming convention is an `_experimental` suffix.
_Avoid_: calling an additive gate a "skip" — it adds, it does not subtract.

**Subtractive gate**:
A feature whose default (absent) state writes the rows, and *enabling* it
*removes* the writes. The default build is the full production build; the feature
exists only to let tests opt out of an expensive write path. Example:
`test_only_skip_txout_set_accumulator` (default builds the txout-set accumulator
rows; enabling skips them and serves the RPC with `FeatureUnavailable`). The
naming convention is a `test_only_skip_` prefix; never enable in production.
_Avoid_: assuming a gate's presence means its rows are present — for a
subtractive gate the relationship is inverted.
