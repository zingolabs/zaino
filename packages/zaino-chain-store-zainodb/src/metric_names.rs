//! Prometheus metric names emitted by this backend.
//!
//! The strings are unchanged from when these metrics lived in `zaino-state`.
//! Renaming one would silently break every dashboard and alert built on it,
//! so a move is never the moment to tidy them.

#![allow(missing_docs)] // names are self-describing; descriptions live in zainod

pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";

pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";
pub const SYNC_LAG_BLOCKS: &str = "zaino.sync.lag_blocks";
pub const SYNC_ITERATIONS_TOTAL: &str = "zaino.sync.iterations_total";
pub const SYNC_ITERATION_DURATION_SECONDS: &str = "zaino.sync.iteration_duration_seconds";
pub const SYNC_ERRORS_TOTAL: &str = "zaino.sync.errors_total";
pub const SYNC_HAS_REACHED_TIP: &str = "zaino.sync.has_reached_tip";
pub const SYNC_REACHED_TIP_AT: &str = "zaino.sync.reached_tip_at";
// Reorg metrics moved to `zaino-chain-head-service`, which is where the
// reorg is now observed. Their strings are unchanged.
pub const SYNC_BLOCK_BUILD_SECONDS: &str = "zaino.sync.block_build_seconds";
pub const SYNC_BLOCK_WRITE_SECONDS: &str = "zaino.sync.block_write_seconds";
pub const SYNC_TRANSACTIONS_TOTAL: &str = "zaino.sync.transactions_total";
pub const SYNC_SAPLING_OUTPUTS_TOTAL: &str = "zaino.sync.sapling_outputs_total";
pub const SYNC_ORCHARD_ACTIONS_TOTAL: &str = "zaino.sync.orchard_actions_total";
pub const SYNC_LAST_BLOCK_WRITTEN_AT: &str = "zaino.sync.last_block_written_at";

pub const DB_TIP_HEIGHT: &str = "zaino.db.tip_height";
pub const FINALISED_EPHEMERAL: &str = "zaino.db.finalised_ephemeral";
pub const ACCUMULATOR_BUILT_HEIGHT: &str = "zaino.db.accumulator_built_height";
pub const ACCUMULATOR_REBUILD_ACTIVE: &str = "zaino.db.accumulator_rebuild_active";

pub const MEMPOOL_TRANSACTIONS: &str = "zaino.mempool.transactions";
pub const MEMPOOL_TIP_CHANGES_TOTAL: &str = "zaino.mempool.tip_changes_total";
