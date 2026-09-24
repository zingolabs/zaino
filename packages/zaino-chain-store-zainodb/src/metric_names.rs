//! Prometheus metric names emitted by this backend, with the `# HELP` `zainod` registers.
//!
//! - Every name here is emitted here, and every metric emitted here is named here
//! - Renaming one breaks every dashboard built on it

#![allow(missing_docs)] // the `# HELP` in the tables below is the description

// Sync lag = `zaino.chain.tip_height` - SYNC_FINALIZED_HEIGHT, consumer-derived
pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
pub const SYNC_FETCHED_HEIGHT: &str = "zaino.sync.fetched_height";
pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";

// Three disjoint spans summing to the per-block cost (under `direct` a fetch = in-process CPU)
pub const SYNC_BLOCK_FETCH_SECONDS: &str = "zaino.sync.block_fetch_seconds";
pub const SYNC_TREESTATE_FETCH_SECONDS: &str = "zaino.sync.treestate_fetch_seconds";
pub const SYNC_BLOCK_ASSEMBLE_SECONDS: &str = "zaino.sync.block_assemble_seconds";

// Insert+sort vs device flush: saturate for unrelated reasons
pub const SYNC_BATCH_WRITE_SECONDS: &str = "zaino.sync.batch_write_seconds";
pub const SYNC_FSYNC_SECONDS: &str = "zaino.sync.fsync_seconds";
pub const SYNC_BATCH_BLOCKS: &str = "zaino.sync.batch_blocks";

pub const SYNC_ACCUMULATOR_SECONDS: &str = "zaino.sync.accumulator_seconds";
pub const SYNC_ACCUMULATOR_HEIGHT: &str = "zaino.sync.accumulator_height";
/// Label on SYNC_ACCUMULATOR_SECONDS: `delta` vs from-genesis `rebuild`
pub const ACCUMULATOR_MODE: &str = "mode";

// Paired with SYNC_FETCHED_HEIGHT, not the commit (per-commit = 120s sawtooth rate)
// - Failed commit or restart re-fetches → re-counts; durable progress = SYNC_FINALIZED_HEIGHT
pub const SYNC_FETCHED_TRANSACTIONS_TOTAL: &str = "zaino.sync.fetched_transactions_total";
pub const SYNC_FETCHED_TRANSPARENT_INPUTS_TOTAL: &str =
    "zaino.sync.fetched_transparent_inputs_total";
pub const SYNC_FETCHED_TRANSPARENT_OUTPUTS_TOTAL: &str =
    "zaino.sync.fetched_transparent_outputs_total";
pub const SYNC_FETCHED_SAPLING_SPENDS_TOTAL: &str = "zaino.sync.fetched_sapling_spends_total";
pub const SYNC_FETCHED_SAPLING_OUTPUTS_TOTAL: &str = "zaino.sync.fetched_sapling_outputs_total";
pub const SYNC_FETCHED_ORCHARD_ACTIONS_TOTAL: &str = "zaino.sync.fetched_orchard_actions_total";
pub const SYNC_FETCHED_IRONWOOD_ACTIONS_TOTAL: &str = "zaino.sync.fetched_ironwood_actions_total";

// No LMDB reader slots: `mdb_env_info` = raw FFI, crate forbids unsafe
pub const DB_MAP_SIZE_BYTES: &str = "zaino.db.map_size_bytes";
pub const DB_USED_BYTES: &str = "zaino.db.used_bytes";

pub const DB_READ_SECONDS: &str = "zaino.db.read_seconds";
pub const DB_CORRUPT_ROWS_TOTAL: &str = "zaino.db.corrupt_rows_total";

#[rustfmt::skip]
pub const COUNTERS: &[(&str, &str)] = &[
    (SYNC_FETCHED_TRANSACTIONS_TOTAL, "Transactions in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_TRANSPARENT_INPUTS_TOTAL, "Transparent inputs in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_TRANSPARENT_OUTPUTS_TOTAL, "Transparent outputs in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_SAPLING_SPENDS_TOTAL, "Sapling spends in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_SAPLING_OUTPUTS_TOTAL, "Sapling outputs in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_ORCHARD_ACTIONS_TOTAL, "Orchard actions in blocks fetched and assembled by the sync loop"),
    (SYNC_FETCHED_IRONWOOD_ACTIONS_TOTAL, "Ironwood actions in blocks fetched and assembled by the sync loop"),
    (DB_CORRUPT_ROWS_TOTAL, "Rows read from the finalized database that could not be decoded"),
];

#[rustfmt::skip]
pub const GAUGES: &[(&str, &str)] = &[
    (SYNC_FINALIZED_HEIGHT, "Height the finalized index is committed and fsynced to"),
    (SYNC_FETCHED_HEIGHT, "Height the sync loop has built to in memory, ahead of the next commit"),
    (SYNC_TARGET_HEIGHT, "Height the write path works towards: chain tip minus the reorg buffer"),
    (SYNC_ACCUMULATOR_HEIGHT, "Height the txout-set accumulator is built to"),
    (DB_MAP_SIZE_BYTES, "Bytes the LMDB map is sized to"),
    (DB_USED_BYTES, "Bytes in use by the LMDB environment"),
];

#[rustfmt::skip]
pub const HISTOGRAMS: &[(&str, &str)] = &[
    (SYNC_BLOCK_FETCH_SECONDS, "Seconds to fetch one block from the source"),
    (SYNC_TREESTATE_FETCH_SECONDS, "Seconds to fetch one block's commitment-tree roots"),
    (SYNC_BLOCK_ASSEMBLE_SECONDS, "Seconds to assemble one fetched block into an indexed block"),
    (SYNC_BATCH_WRITE_SECONDS, "Seconds to write one block batch into the B-tree, excluding the fsync"),
    (SYNC_FSYNC_SECONDS, "Seconds in the LMDB checkpoint fsync after a batch write"),
    (SYNC_BATCH_BLOCKS, "Blocks in one assembled write batch"),
    (SYNC_ACCUMULATOR_SECONDS, "Seconds bringing the txout-set accumulator to the tip, by mode"),
    (DB_READ_SECONDS, "Seconds to serve one finalized-database read, by op"),
];
