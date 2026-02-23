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
DB VERSION v1.1.0 (from v1.0.0)
Date: 2026-01-27
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

Migration
- Strategy: In-place (metadata-only).
- Backfill: None.
- Completion criteria:
  - DbMetadata.version updated from 1.0.0 to 1.1.0.
  - DbMetadata.migration_status reset to Empty.
- Failure handling:
  - Idempotent: re-running re-writes the same metadata; no partial state beyond metadata.

Bug Fixes / Optimisations
- Added safety check for idempotent DB writes

--------------------------------------------------------------------------------
(append new entries below)
--------------------------------------------------------------------------------
