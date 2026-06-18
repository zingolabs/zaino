Zaino Finalised-State Database Changelog
=======================================

Format
------
One entry per database version bump (major / minor / patch). Keep entries concise and factual.

Entry template:

--------------------------------------------------------------------------------
DB VERSION vX.Y.Z (from vA.B.C)
Date: YYYY-MM-DD
--------------------------------------------------------------------------------

Summary
- <1–3 bullets describing intent of the change>

On-disk schema
- Layout:
  - <directory / file layout changes>
- Tables:
  - Added: <...>
  - Removed: <...>
  - Renamed: <old -> new>
- Encoding:
  - Keys: <what changed, if anything>
  - Values: <what changed, if anything>
  - Checksums / validation: <what changed, if anything>
- Invariants:
  - <new or changed integrity constraints>

API / capabilities
- Capability changes:
  - Added: <...>
  - Removed: <...>
  - Changed: <...>
- Public surface changes:
  - Added: <methods / behaviors>
  - Removed: <methods / behaviors>
  - Changed: <semantic changes, error mapping changes>

Migration
- Strategy: <in-place | shadow build | rebuild>
- Backfill: <what gets rebuilt and how broadly>
- Completion criteria: <how we decide migration is done>
- Failure handling: <rollback / retry behavior>

Bug Fixes / Optimisations

--------------------------------------------------------------------------------
DB VERSION v1.0.0 (from v0.0.0)
Date: 2025-08-13
--------------------------------------------------------------------------------

Summary
- Replace legacy v0 schema with versioned v1 schema and expanded indices / query surface.
- Introduce stronger integrity checks and on-demand validation for v1 read paths.
- Keep compact block retrieval available (compatibility surface).

On-disk schema
- Layout:
  - Move to per-network version directory layout: <base>/<network>/v1/
  - VERSION_DIRS begins at ["v1"] (new versions append, no gaps).
- Tables:
  - Added (v1): headers, txids, transparent, sapling, orchard, commitment_tree_data, heights (hash->height),
    plus v1 indices for tx locations, spent outpoints, and transparent address history.
  - Removed / superseded (v0): legacy compact-block-streamer oriented storage layout.
- Encoding:
  - v1 values are stored as checksum-protected `StoredEntryVar<T>` / `StoredEntryFixed<T>` entries.
  - Canonical key bytes are used for checksum verification via `verify(key)`.
- Invariants (v1 validation enforces):
  - Per-table checksum verification for all per-block tables.
  - Chain continuity: header parent hash at height h matches stored hash at h-1.
  - Merkle consistency: header merkle root matches computed root from stored txid list.
  - Index consistency:
    - hash->height mapping must match the queried height.
    - spent + addr history records must exist and match for transparent inputs/outputs.

API / capabilities
- Capability changes:
  - v0: READ_CORE | WRITE_CORE | COMPACT_BLOCK_EXT
  - v1: Capability::LATEST (block core/transparent/shielded, indexed block, transparent history, etc.)
- Public surface changes:
  - Added (v1-only; FeatureUnavailable on v0):
    - BlockCoreExt: header/txids/range fetch, txid<->location lookup
    - BlockTransparentExt: per-tx and per-block transparent access + ranges
    - BlockShieldedExt: sapling/orchard per-tx and per-block access + ranges, commitment tree data (+ ranges)
    - IndexedBlockExt: indexed block retrieval
    - TransparentHistExt: addr records, range queries, balance/utxos, outpoint spender(s)
  - Preserved:
    - CompactBlockExt remains available for both v0 and v1.

Migration
- Strategy: shadow build + promotion (no in-place transformation of v0).
- Backfill: rebuild all v1 tables/indices by ingesting chain data.
- Completion criteria:
  - metadata indicates migrated/ready, and required tables exist through the tip.
  - validation succeeds for the contiguous best chain range as built.
- Failure handling:
  - do not promote partially built v1; continue using v0 if present; rebuild v1 on retry.

Bug Fixes / Optimisations
- Complete DB rework
--------------------------------------------------------------------------------
DB VERSION v1.0.0 (RC Bug Fixes)
--------------------------------------------------------------------------------

Summary
- Minor version bump to reflect updated compact block API contract (streaming + pool filtering semantics).
- No schema or encoding changes; metadata-only migration updates persisted DB version marker.

On-disk schema
- Layout:
  - No changes.
- Tables:
  - Added: None.
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys: No changes.
  - Values: No changes.
  - Checksums / validation: No changes.
- Invariants:
  - No changes.

API / capabilities
- Capability changes:
  - Added: None.
  - Removed: None.
  - Changed:
    - COMPACT_BLOCK_EXT contract updated for v1 backends:
      - get_compact_block(...) now takes a PoolTypeFilter, which selects which pool data is materialized into the returned compact block.
      - get_compact_block_stream(...) added.

- Public surface changes:
  - Added:
    - CompactBlockExt::get_compact_block_stream(start_height, end_height, pool_types: PoolTypeFilter).
  - Removed: None.
  - Changed:
    - CompactBlockExt::get_compact_block(height, pool_types: PoolTypeFilter) signature updated.
    - Compact block contents are now filtered by PoolTypeFilter, and may include transparent transaction data (vin/vout) when selected.

Bug Fixes / Optimisations
- Added safety check for idempotent DB writes
- Updated 'fix_addr_hist_records_by_addr_and_index_blocking' to take and reuse an lmdb ro transaction, improving initial sync performance.

--------------------------------------------------------------------------------
DB VERSION v1.0.0 (from v1.1.0)
Date: 2026-01-27
--------------------------------------------------------------------------------

Summary
- BlockHeaderData v2 introduced (internally using new BlockIndex::V2 format); because relevant tables (notably `headers` / `BlockHeaderData`) use
   variable-length encodings existing tables are updated in-place: DB values may contain either v1 or v2 `BlockHeaderData` entries.
- Recorded on-disk schema text was clarified; migration refreshes persisted `DbMetadata.schema_hash`
   so the metadata matches the repository's schema contract.

On-disk schema
- Layout:
  - Updated [`BlockHeaderData`] table by introducing [`BlockHeaderData::V2`] (and internally [`BlockIndex::V2`]), this table may now hold either V1 or V2
     [`BlockHeaderData`] structs, with serde handled internally.
- Tables:
  - Added: None.
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys: No changes.
  - Values: Introduced `[BlockHeaderData::V2]`.
  - Checksums / validation: No changes.
- Invariants:
  - No changes.

--------------------------------------------------------------------------------
DB VERSION v1.2.0 (from v1.1.0)
Date: 2026-06-11
--------------------------------------------------------------------------------

Summary
- Promote the `spent` outpoint index to core finalised-state data.
- Add a finalised txout-set accumulator (`tx_out_set_info_accumulator`)
  maintaining the data needed to serve `gettxoutsetinfo` directly from the
  indexer.
- Add a reverse transaction-id index (`txid_location`, `txid -> TxLocation`)
  so previous-output resolution is an O(log n) point lookup instead of a full
  scan of the height-keyed `txids` table. This fixes a near-quadratic slowdown
  in both the migration backfill and clean-sync write path.
- Backfill the new structures from existing per-block transparent transaction
  data via a single in-place, three-stage migration (`txid_location`, then
  `spent`, then a bulk txout-set accumulator rebuild).
- Add resumable in-place migration progress tracking using temporary metadata
  entries (one per backfill stage).
- The random-keyed `spent` / `txid_location` indexes are written in sorted
  batches (sequential B-tree sweep instead of a random fault per insert once the
  DB exceeds RAM), and the txout-set accumulator is rebuilt in bulk from the
  finalised tables rather than maintained per block.

On-disk schema
- Layout:
  - No directory layout changes.
- Tables:
  - Added: `spent` is now a core v1 table rather than an experimental transparent-address-history table.
  - Added: `tx_out_set_info_accumulator` — singleton table holding the
    finalised transparent UTXO-set summary
    (LMDB database name: `tx_out_set_info_accumulator_1_2_0`,
    singleton key: ASCII `"tx_out_set_info_accumulator"`).
  - Added: `txid_location` — reverse transaction-id index mapping each
    transaction id to its on-chain `TxLocation`
    (LMDB database name: `txid_location_1_0_0`).
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys: No changes to `Outpoint` encoding. `txid_location` is keyed by the
    32-byte transaction id (internal byte order).
  - Values: `spent` stores `StoredEntryFixed<TxLocation>` values.
    `txid_location` stores `StoredEntryFixed<TxLocation>` values
    (checksum-protected against the txid key).
    `tx_out_set_info_accumulator` stores
    `StoredEntryFixed<FinalisedTxOutSetInfoAccumulator>` whose body is
    `LE(u64) transactions || LE(u64) transaction_outputs ||
     LE(u64) bytes_serialized || [32] hash_serialized ||
     LE(u64) total_zatoshis` (64 bytes).
  - Checksums / validation:
    - `spent` entries are checksum-protected using the encoded `Outpoint` key.
    - `txid_location` entries are checksum-protected using the txid key.
    - The accumulator entry is checksum-protected using its singleton key.
    - The height the accumulator was last fully built to is recorded in the
      metadata DB under `_tx_out_set_accumulator_built_height` as
      `StoredEntryFixed<Height>` — a freshness watermark letting readers detect
      a stale accumulator after a sync was interrupted before its bulk rebuild.
    - Migration progress is temporarily stored as `StoredEntryFixed<Height>` in
      the metadata DB under `_migration_spent_progress_1_2_0_next_height`
      (Stage B) and `_migration_txid_location_progress_1_2_0_next_height`
      (Stage A).
- Invariants:
  - For every non-null transparent input in finalised-state block data, `spent[Outpoint]` must exist and point to the spending transaction’s `TxLocation`.
  - For every transaction in finalised-state block data, `txid_location[txid]`
    must exist and resolve to that transaction’s `TxLocation`.
  - Existing `spent` / `txid_location` entries encountered during migration must decode, verify, and match the expected `TxLocation`.
  - The accumulator excludes provably-unspendable transparent outputs
    (anything whose `ScriptType` is not `P2PKH` or `P2SH` — matches zcashd's
    `IsUnspendable()` view of the UTXO set: OP_RETURN, oversized scripts,
    etc.). `bytes_serialized == transaction_outputs * 65` by construction.
  - `hash_serialized` is the XOR over all currently-unspent transparent
    outputs of `BLAKE2b-256(b"ZcashTxOutSet___" || prev_txid || LE(u32)
    vout || LE(u64) value || script_hash[20] || u8 script_type)`. Order-
    independent and self-inverse under add/remove. Not byte-equal to
    zcashd's `hash_serialized`.

API / capabilities
- Capability changes:
  - Added: core availability of spent-outpoint lookup data.
  - Removed: None.
  - Changed:
    - Spent-outpoint indexing is no longer dependent on transparent address-history support.
    - `BlockTransparentExt::get_previous_output` is now part of the trait
      (formerly available only behind
      `transparent_address_history_experimental`).
    - `TransparentHistExt::get_tx_out_set_info_accumulator` returns the new
      `FinalisedTxOutSetInfoAccumulator` value.
- Public surface changes:
  - Added:
    - `DbReader::get_previous_output(outpoint) -> TxOutCompact` — the
      read-only entry point for finalised previous-output lookups.
    - `DbReader::get_tx_out_set_info_accumulator() -> FinalisedTxOutSetInfoAccumulator`.
  - Removed: None.
  - Changed:
    - Existing spent/outpoint-spender functionality can be backed by the core `spent` table.

Migration
- Strategy: in-place index backfill, run as three sequential stages
  (single migration step, re-entrant).
- Backfill:
  - Stage A (`txid_location`): scans the existing `txids` table from genesis
    through the current finalised DB tip and writes
    `txid -> StoredEntryFixed<TxLocation>`.
  - Stage B (`spent`): iterates existing transparent block data and writes
    `Outpoint -> StoredEntryFixed<TxLocation>` for each non-null transparent
    input. Entries are buffered and inserted in sorted key order in batches
    (bounded by `storage.database.sync_write_batch_bytes`); each batch commits
    its `spent` entries together with the Stage B progress watermark in one
    transaction.
  - Stage C (`tx_out_set_info_accumulator`): rebuilds the accumulator in bulk
    from the finalised `transparent` + `spent` tables via sequential scans
    (skipping NonStandard outputs) and overwrites the singleton. It never trusts
    an existing accumulator — so a partially-run prior (2-stage) migration is
    recomputed correctly rather than corrupted — and is idempotent / re-runnable,
    so it needs no per-height progress key.
- 0.4.0-alpha.1 compatibility (temporary):
  - A cache built by 0.4.0-alpha.1 is recorded at v1.2.0 but has an empty
    `txid_location` index. On open, a non-empty database at version >= 1.2.0
    with an empty `txid_location` table has its `spent` table cleared and its
    recorded version rolled back to 1.1.0 so this migration rebuilds the
    indices in place (rather than forcing a full rebuild from the validator).
    This shim is to be removed once 0.4.0 ships.
- Completion criteria:
  - All heights through the current finalised DB tip have been processed by both stages.
  - Migration status reaches `Complete`.
  - Both temporary migration progress keys are deleted.
  - `DbMetadata.version` is advanced to v1.2.0 and `migration_status` is reset to `Empty`.
- Failure handling:
  - Stages A and B resume from their own temporary metadata progress heights.
  - `spent` entries and the Stage B progress watermark commit together per batch;
    `txid_location` entries and their Stage A progress likewise. The watermark is
    never advanced past committed data, so a crash resumes from the last committed
    batch (re-doing only uncommitted heights).
  - Stage C is a separate idempotent bulk rebuild, so on resume the accumulator
    is recomputed from the (already-built) finalised tables rather than tracked
    per height.
  - Existing matching `spent` / `txid_location` entries are accepted after checksum and `TxLocation` verification.
  - Existing conflicting or corrupt entries fail the migration.

Bug Fixes / Optimisations
- Reverse txid lookups (`find_txid_index_blocking`, and therefore
  `get_tx_location` / `get_previous_output`) are now O(log n) point lookups on
  `txid_location` instead of a full cursor scan of `txids`. This removes a
  near-quadratic cost that made the v1.1.0 -> v1.2.0 migration appear to hang
  on large caches and progressively slowed clean sync.
- The v1.1.0 -> v1.2.0 migration now logs per-stage start/completion and
  periodic progress (height / db tip / elapsed).
- `write_block` no longer issues two redundant `env.sync(true)` calls around
  per-block validation; the durable `txn.commit()` already fsyncs, so crash
  safety is unchanged.
- The txout-set accumulator is no longer maintained per block during bulk sync /
  migration: it is deferred and rebuilt once at the tip, removing an unbounded
  fan-out of random `spent` reads per block that stalled sync around sandblast
  height. Single-block appends still maintain it incrementally.
- Block validation is off the write hot path: writes do cheap in-memory
  parent-hash + merkle-root checks and advance `validated_tip` directly; the full
  read-back validation runs only at startup.
- Startup validation scans in ascending height order (was block-hash order via
  `heights`), so `validated_tip` advances monotonically and startup avoids
  random-access cache thrash.
- The random-keyed `spent` / `txid_location` writes (clean sync and migration)
  are batched and inserted in sorted key order, turning per-insert random B-tree
  faults into a sequential sweep once the DB exceeds RAM. Tunable via
  `storage.database.sync_write_batch_bytes` (default 4 GiB).

Bug Fixes / Optimisations
- Avoids a shadow rebuild by deriving the new core `spent` index from existing transparent transaction data.
- Avoids temporary named LMDB databases by storing migration progress as a temporary metadata entry.

--------------------------------------------------------------------------------
(append new entries below)
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
DB VERSION v1.2.1 (from v1.2.0)
Date: 2026-06-10
--------------------------------------------------------------------------------

Summary
- Metadata-only version marker for the optional ("ephemeral") finalised state and
  background (non-blocking) finalised-state sync / migration behaviour.
- No on-disk layout, table, encoding, checksum, or schema-hash change: persisted
  v1.2.1 databases are byte-for-byte compatible with v1.2.0.

On-disk schema
- Layout:
  - No changes.
- Tables:
  - Added: None.
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys: No changes.
  - Values: No changes.
  - Checksums / validation: No changes (`DB_SCHEMA_V1_HASH` unchanged).
- Invariants:
  - No changes.

API / capabilities
- Capability changes:
  - Added: None.
  - Removed: None.
  - Changed: None.
- Public surface changes:
  - The finalised-state backing enum was renamed `DbBackend` -> `FinalisedSource`
    (variant `Stateless` -> `Ephemeral`) and its module `db` -> `finalised_source`,
    reflecting that the backing is not necessarily a database (it may be an
    ephemeral passthrough). The facade type `ZainoDB` was renamed `FinalisedState`.
    These are internal (`pub(crate)`) renames with no external API impact.
  - Added: `FinalisedState::wait_until_synced` (waits for background sync/migration
    to reach its target, distinct from `wait_until_ready`'s serving-readiness).

Migration
- Strategy: metadata-only (default `Migration` trait implementation), run against
  the routed primary. No shadow database, no table rebuild.
- Backfill: None.
- Completion criteria: `DbMetadata.version` is advanced to v1.2.1 and
  `migration_status` is reset to `Empty`; the schema checksum is re-stamped unchanged.
- Failure handling: idempotent; safe to re-run.

Bug Fixes / Optimisations
- Finalised-state sync and migration no longer block serving: large syncs and
  version migrations run in the background while an ephemeral passthrough serves
  finalised reads from the backing source.
