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
Date: 2026-05-11
--------------------------------------------------------------------------------

Summary
- Add two new LMDB tables to the v1 schema for the logical-timestamp
  index that backs zcashd-parity `getblockhashes` (zingolabs/zaino#1101):
  - `hash_by_logical_ts_1_0_0`  — `LogicalTimestamp -> BlockHash`
    (forward; one cursor scan answers a `getblockhashes` range query).
  - `logical_ts_by_hash_1_0_0`  — `BlockHash -> LogicalTimestamp`
    (reverse; needed for `delete_block` cleanup and for cheap
    parent-lookup when `write_block` extends the chain).
- Backfill the new tables over every header already in `headers` by
  replaying `LogicalTimestamp::next` in height order.

On-disk schema
- Layout:
  - No layout/directory changes; the new tables live in the existing
    `<network>/v1/` LMDB environment.
- Tables:
  - Added: `hash_by_logical_ts_1_0_0`, `logical_ts_by_hash_1_0_0`.
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys for `hash_by_logical_ts_1_0_0`: 5 bytes —
    `version_tag(0x01) + BE u32 logical_ts`. Big-endian so cursor
    iteration is in ascending numeric order.
  - Values for `hash_by_logical_ts_1_0_0`: `StoredEntryFixed<BlockHash>`.
  - Keys for `logical_ts_by_hash_1_0_0`: 32-byte block hash.
  - Values for `logical_ts_by_hash_1_0_0`:
    `StoredEntryFixed<LogicalTimestamp>`.
  - Checksums / validation: both tables use the standard
    `StoredEntryFixed` checksum scheme keyed by the LMDB key bytes.
- Invariants:
  - Forward and reverse indices stay consistent: every entry in one
    has a matching entry in the other. Enforced by `write_block`
    (writes both atomically) and `delete_block` (deletes both
    atomically).
  - `logical_ts` is strictly monotonic across finalised height order.
  - LMDB `max_dbs` raised from 12 to 14 to leave margin above the
    active table count under both `transparent_address_history_experimental`
    feature configurations.

API / capabilities
- Capability changes:
  - Added: None (the new tables ride on the existing
    `BLOCK_CORE_EXT` capability).
  - Removed: None.
  - Changed: None.
- Public surface changes:
  - Added (on `BlockCoreExt`, dispatched by `DbBackend` to V1):
    - `hashes_by_logical_ts_range(low, high)` — returns
      `Vec<(LogicalTimestamp, BlockHash)>` from the forward index over
      the half-open range `[low, high)`. Matches zcashd's
      `getblockhashes` semantics.
  - Removed: None.
  - Changed: None.

Migration
- Strategy: in-place backfill within the existing v1 LMDB environment.
- Backfill: walk every header already in `headers` in height order,
  replay `LogicalTimestamp::next`, populate both index tables in one
  rw transaction. After the populate step commits, advance
  `DbMetadata.version` to `1.2.0` and refresh `DbMetadata.schema_hash`
  to the BLAKE2b-256 of the updated `db_schema_v1.txt`.
- Completion criteria:
  - `DbMetadata.version == {1, 2, 0}`,
  - `DbMetadata.schema_hash == DB_SCHEMA_V1_HASH`,
  - both index tables have one entry per finalised block.
- Failure handling: the populate step runs in a single LMDB rw
  transaction so a crash either fully populates the indices or leaves
  them empty. On retry, the next launch detects current_version ==
  1.1.0 and re-runs the migration; `txn.put` uses
  `WriteFlags::empty` (overwriting allowed) so any committed-then-
  aborted prior attempt re-writes with the same deterministic value.

Bug Fixes / Optimisations
- New `BlockCoreExt::hashes_by_logical_ts_range` lets `getblockhashes`
  answer range queries via one O(matches) cursor scan instead of the
  O(N) genesis walk previously required. The actual consumer in
  `chain_index.rs::get_block_hashes` is wired up in a follow-up
  commit that ships once this migration is in place.

--------------------------------------------------------------------------------
(append new entries below)
--------------------------------------------------------------------------------
