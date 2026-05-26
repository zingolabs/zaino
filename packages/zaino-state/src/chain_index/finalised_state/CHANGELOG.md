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
Date: 2026-05-10
--------------------------------------------------------------------------------

Summary
- Promote the `spent` outpoint index to core finalised-state data.
- Add a finalised txout-set accumulator (`tx_out_set_info_accumulator`)
  maintaining the data needed to serve `gettxoutsetinfo` directly from the
  indexer.
- Backfill both new structures from existing per-block transparent transaction
  data via a single in-place migration.
- Add resumable in-place migration progress tracking using a temporary metadata entry.

On-disk schema
- Layout:
  - No directory layout changes.
- Tables:
  - Added: `spent` is now a core v1 table rather than an experimental transparent-address-history table.
  - Added: `tx_out_set_info_accumulator` — singleton table holding the
    finalised transparent UTXO-set summary
    (LMDB database name: `tx_out_set_info_accumulator_1_2_0`,
    singleton key: ASCII `"tx_out_set_info_accumulator"`).
  - Removed: None.
  - Renamed: None.
- Encoding:
  - Keys: No changes to `Outpoint` encoding.
  - Values: `spent` stores `StoredEntryFixed<TxLocation>` values.
    `tx_out_set_info_accumulator` stores
    `StoredEntryFixed<FinalisedTxOutSetInfoAccumulator>` whose body is
    `LE(u64) transactions || LE(u64) transaction_outputs ||
     LE(u64) bytes_serialized || [32] hash_serialized ||
     LE(u64) total_zatoshis` (64 bytes).
  - Checksums / validation:
    - `spent` entries are checksum-protected using the encoded `Outpoint` key.
    - The accumulator entry is checksum-protected using its singleton key.
    - Migration progress is temporarily stored as `StoredEntryFixed<Height>` in the metadata DB under `_migration_spent_progress_1_2_0_next_height`.
- Invariants:
  - For every non-null transparent input in finalised-state block data, `spent[Outpoint]` must exist and point to the spending transaction’s `TxLocation`.
  - Existing `spent` entries encountered during migration must decode, verify, and match the expected `TxLocation`.
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
- Strategy: in-place index backfill (single migration step covers both new
  structures).
- Backfill:
  - Iterates existing transparent block data from genesis through the current finalised DB tip.
  - For each non-null transparent input, writes `Outpoint -> StoredEntryFixed<TxLocation>` to `spent`.
  - For each block, advances the singleton
    `tx_out_set_info_accumulator` via the same calculator used by the
    write path (`calculate_tx_out_set_info_accumulator_after_block`),
    skipping NonStandard outputs.
- Completion criteria:
  - All heights through the current finalised DB tip have been processed.
  - Migration status reaches `Complete`.
  - Temporary migration progress key is deleted.
  - `DbMetadata.version` is advanced to v1.2.0 and `migration_status` is reset to `Empty`.
- Failure handling:
  - Resumes from the temporary metadata progress height.
  - Spent entries and progress updates for each height are committed in the same LMDB transaction.
  - Existing matching spent entries are accepted after checksum and `TxLocation` verification.
  - Existing conflicting or corrupt spent entries fail the migration.

Bug Fixes / Optimisations
- Avoids a shadow rebuild by deriving the new core `spent` index from existing transparent transaction data.
- Avoids temporary named LMDB databases by storing migration progress as a temporary metadata entry.

--------------------------------------------------------------------------------
(append new entries below)
--------------------------------------------------------------------------------
