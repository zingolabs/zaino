//! Prometheus metric names emitted by this backend.
//!
//! The strings are unchanged from when these metrics lived in `zaino-state`.
//! Renaming one would silently break every dashboard and alert built on it,
//! so a move is never the moment to tidy them.
//!
//! # Only what this crate emits
//!
//! Every name here is emitted from this crate, and every metric this crate
//! emits is named here. The set arrived as a wholesale copy of `zaino-state`'s,
//! which left two problems behind: names for the sync loop, mempool and chain
//! head — subsystems this backend does not observe — implied emission that does
//! not happen, and `pub const` kept the compiler quiet about it; and the names
//! that *did* move were then defined in both crates, so `zaino-state` still
//! held a copy of a string only this crate emits. A renamed metric there would
//! have broken dashboards while every pin test still passed, because the pinned
//! copy was no longer the live one.
//!
//! `zaino-state` re-exports these rather than restating them, so a consumer
//! still has one import site and there is one definition to rename.

#![allow(missing_docs)] // names are self-describing; descriptions live in zainod

pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";
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

// Read path. The write path has been instrumented since before the move; these
// are the reads, which had nothing. A wallet syncing against this store spends
// almost all of its time in `compact_chunk`, so a store that is slow or rotting
// was invisible from a dashboard while every write metric looked healthy.
pub const DB_BLOCK_READ_SECONDS: &str = "zaino.db.block_read_seconds";
pub const DB_COMPACT_READ_SECONDS: &str = "zaino.db.compact_read_seconds";
pub const DB_CORRUPT_ROWS_TOTAL: &str = "zaino.db.corrupt_rows_total";
