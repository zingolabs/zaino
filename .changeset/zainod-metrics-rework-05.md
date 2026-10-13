---
default: major
---

# `zaino-chain-store-zainodb`: Sync and store metrics reworked

Sync and store metrics reworked. Added: `zaino.sync.fetched_height`, `block_fetch_seconds`, `treestate_fetch_seconds`, `block_assemble_seconds`, `batch_write_seconds`, `fsync_seconds`, `batch_blocks`, `accumulator_seconds{mode}`, `accumulator_height`, `zaino.sync.fetched_{transactions,transparent_inputs,transparent_outputs,sapling_spends,sapling_outputs,orchard_actions,ironwood_actions}_total`, `zaino.db.validated_height`, `validation_seconds`, `on_demand_validations_total`, `map_size_bytes`, `used_bytes`, `zaino.migration.progress_height`. `zaino.sync.finalized_height` now means committed and fsynced.

**Migration:** Removed: `zaino.sync.block_build_seconds` (sum of the three per-block spans), `zaino.sync.block_write_seconds` (→ `batch_write_seconds` + `fsync_seconds`), `zaino.sync.{transactions,sapling_outputs,orchard_actions}_total` (→ `fetched_*_total`), `zaino.sync.last_block_written_at`, `zaino.db.tip_height` (→ `zaino.sync.finalized_height`), `zaino.db.finalised_ephemeral`, `zaino.db.accumulator_built_height` (→ `zaino.sync.accumulator_height`), `zaino.db.accumulator_rebuild_active` (rebuild progress is logged). Throughput counters count fetched blocks: a failed commit or restart re-counts. Feature `prometheus` removed; emission is unconditional (no-op without a recorder).
