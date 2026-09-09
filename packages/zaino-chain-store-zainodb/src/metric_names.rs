//! Prometheus metric names emitted by this backend, each with its `# HELP`.
//!
//! - Every name here is emitted here, and every metric emitted here is named here
//! - Renaming one breaks every dashboard built on it, so a move is never the moment to tidy

#![allow(missing_docs)] // the `# HELP` beside each name is the description

zaino_status::metric_names! {
    // Sync lag = `zaino.chain.tip_height` - SYNC_FINALIZED_HEIGHT, consumer-derived
    gauge SYNC_FINALIZED_HEIGHT = "zaino.sync.finalized_height" => "Height the finalized index is committed and fsynced to";
    gauge SYNC_FETCHED_HEIGHT = "zaino.sync.fetched_height" => "Height the sync loop has built to in memory, ahead of the next commit";
    gauge SYNC_TARGET_HEIGHT = "zaino.sync.target_height" => "Height the write path works towards: chain tip minus the reorg buffer";

    // Behind the finalised tip: reads above validated_height pay a synchronous re-read
    gauge DB_VALIDATED_HEIGHT = "zaino.db.validated_height" => "Height the finalized index is structurally validated to";
    histogram DB_VALIDATION_SECONDS = "zaino.db.validation_seconds" => "Seconds to structurally re-validate one stored block";
    counter DB_ON_DEMAND_VALIDATIONS_TOTAL = "zaino.db.on_demand_validations_total" => "Reads that took the synchronous re-validation path; `db_validation_seconds` counts the ones that did work";

    // DB vs passthrough: different latency & correctness, otherwise indistinguishable
    gauge ROUTER_EPHEMERAL_MODE = "zaino.router.ephemeral_mode" => "Ephemeral routing: 0 none, 1 read-only, 2 full (migration)";
    gauge MIGRATION_ACTIVE = "zaino.migration.active" => "1 while a migration holds full ephemeral routing";
    gauge MIGRATION_PROGRESS_HEIGHT = "zaino.migration.progress_height" => "Height an in-progress migration backfill has reached";

    // Three disjoint spans summing to the per-block cost; under `direct` a "fetch" is
    // in-process CPU, not a validator round trip
    histogram SYNC_BLOCK_FETCH_SECONDS = "zaino.sync.block_fetch_seconds" => "Seconds to fetch one block from the source";
    histogram SYNC_TREESTATE_FETCH_SECONDS = "zaino.sync.treestate_fetch_seconds" => "Seconds to fetch one block's commitment-tree roots";
    histogram SYNC_BLOCK_ASSEMBLE_SECONDS = "zaino.sync.block_assemble_seconds" => "Seconds to assemble one fetched block into an indexed block";

    // Insert+sort vs device flush: they saturate for unrelated reasons
    histogram SYNC_BATCH_WRITE_SECONDS = "zaino.sync.batch_write_seconds" => "Seconds to write one block batch into the B-tree, excluding the fsync";
    histogram SYNC_FSYNC_SECONDS = "zaino.sync.fsync_seconds" => "Seconds in the LMDB checkpoint fsync after a batch write";
    histogram SYNC_BATCH_BLOCKS = "zaino.sync.batch_blocks" => "Blocks in one assembled write batch";

    // O(range) delta vs from-genesis rebuild, which share a distribution without `mode`
    histogram SYNC_ACCUMULATOR_SECONDS = "zaino.sync.accumulator_seconds" => "Seconds bringing the txout-set accumulator to the tip, by mode";
    gauge SYNC_ACCUMULATOR_HEIGHT = "zaino.sync.accumulator_height" => "Height the txout-set accumulator is built to";

    // Directions apart — only outputs are checkable against the note-commitment trees
    counter SYNC_TRANSACTIONS_TOTAL = "zaino.sync.transactions_total" => "Transactions ingested";
    counter SYNC_TRANSPARENT_INPUTS_TOTAL = "zaino.sync.transparent_inputs_total" => "Transparent inputs ingested";
    counter SYNC_TRANSPARENT_OUTPUTS_TOTAL = "zaino.sync.transparent_outputs_total" => "Transparent outputs ingested";
    counter SYNC_SAPLING_SPENDS_TOTAL = "zaino.sync.sapling_spends_total" => "Sapling spends ingested";
    counter SYNC_SAPLING_OUTPUTS_TOTAL = "zaino.sync.sapling_outputs_total" => "Sapling outputs ingested";
    counter SYNC_ORCHARD_ACTIONS_TOTAL = "zaino.sync.orchard_actions_total" => "Orchard actions ingested";
    counter SYNC_IRONWOOD_ACTIONS_TOTAL = "zaino.sync.ironwood_actions_total" => "Ironwood actions ingested";

    gauge FINALISED_EPHEMERAL = "zaino.db.finalized_ephemeral" => "1 while finalised reads are served by the ephemeral passthrough";
    gauge ACCUMULATOR_REBUILD_ACTIVE = "zaino.db.accumulator_rebuild_active" => "1 while a from-genesis accumulator rebuild is running";

    // No LMDB reader slots: `mdb_env_info` = raw FFI, crate forbids unsafe
    gauge DB_MAP_SIZE_BYTES = "zaino.db.map_size_bytes" => "Bytes the LMDB map is sized to";
    gauge DB_USED_BYTES = "zaino.db.used_bytes" => "Bytes in use by the LMDB environment";

    histogram DB_READ_SECONDS = "zaino.db.read_seconds" => "Seconds to serve one finalized-database read, by op";
    counter DB_CORRUPT_ROWS_TOTAL = "zaino.db.corrupt_rows_total" => "Rows read from the finalized database that could not be decoded";
}

/// Accumulator pass: delta vs from-genesis rebuild
pub const ACCUMULATOR_MODE: &str = "mode";
