//! Zaino's core mempool and chain-fetching Library.
//!
//! Built to use a configurable backend:
//! - FetchService
//!    - Built using the Zcash Json RPC Services for backwards compatibility with Zcashd and other JsonRPC based validators.
//! - StateService
//!    - Built using Zebra's ReadStateService for efficient chain access.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::future::Future;

/// A [`Future`] that is [`Send`] and resolves to `T`.
///
/// Written as `impl SendFut<T>` in trait method return positions so the `Send`
/// bound the `async-trait` macro previously supplied implicitly is stated
/// explicitly per method. See `docs/adr/0002-native-afit-over-async-trait.md`.
pub trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}

/// Prometheus metric names emitted by this crate; the single source of truth shared with `zainod`'s `describe_*` registrations (which carry the descriptions).
///
/// - Ungated, unlike the emission: `ingest::observe` takes a name as an ordinary
///   argument, so call sites mention one outside any `cfg`
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    // Progress. lag = CHAIN_TIP_HEIGHT - SYNC_FINALIZED_HEIGHT, consumer-derived
    // (see the sync loop in `chain_index.rs` re: the old gauge)
    pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";
    pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
    pub const SYNC_FETCHED_HEIGHT: &str = "zaino.sync.fetched_height";
    pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";

    // Frontiers behind the finalised tip, each with its own symptom: validated <
    // finalized = reads above pay a sync re-read; accumulator < finalized =
    // gettxoutsetinfo correct only up to it
    pub const DB_VALIDATED_HEIGHT: &str = "zaino.db.validated_height";
    pub const DB_VALIDATION_SECONDS: &str = "zaino.db.validation_seconds";
    pub const DB_ON_DEMAND_VALIDATIONS_TOTAL: &str = "zaino.db.on_demand_validations_total";

    // Liveness = "is it moving", which throughput cannot answer (wedged and idle
    // both publish flat counters). Status gauge lives with its emitter, in
    // `zaino_status::metric_names::STATUS`
    pub const SYNC_ITERATIONS_TOTAL: &str = "zaino.sync.iterations_total";
    pub const SYNC_CONSECUTIVE_FAILURES: &str = "zaino.sync.consecutive_failures";
    pub const SYNC_BACKOFF_SECONDS: &str = "zaino.sync.backoff_seconds";

    // Read routing: persistent DB vs passthrough while syncing/migrating.
    // Different latency, validator load & correctness guarantees, and otherwise
    // indistinguishable from outside the process
    pub const ROUTER_EPHEMERAL_MODE: &str = "zaino.router.ephemeral_mode";

    /// Routed capability resolutions, by [`READ_CAPABILITY`] & [`READ_BACKEND`].
    ///
    /// - Named for the router, not for reads: writes route through the same call,
    ///   so "reads" would make the family total a denominator hiding something else
    /// - Filter on `capability` for the read surface alone
    pub const FINALISED_ROUTED_TOTAL: &str = "zaino.finalised.routed_total";
    pub const MIGRATION_ACTIVE: &str = "zaino.migration.active";
    pub const MIGRATION_PROGRESS_HEIGHT: &str = "zaino.migration.progress_height";

    // Ingest cost: three disjoint spans summing to the per-block cost, nothing
    // recovered by subtraction (see `chain_index::ingest`). None name the
    // validator — under `direct` the read is in-process CPU. All carry
    // [`INGEST_STAGE`]; counts need not match (reorg rebuilds skip the fetch)
    pub const SYNC_BLOCK_FETCH_SECONDS: &str = "zaino.sync.block_fetch_seconds";
    pub const SYNC_TREESTATE_FETCH_SECONDS: &str = "zaino.sync.treestate_fetch_seconds";
    pub const SYNC_BLOCK_ASSEMBLE_SECONDS: &str = "zaino.sync.block_assemble_seconds";

    /// Source reads producing no block, by stage / read / outcome.
    ///
    /// - Keeps the timing histograms pure: NFS is `while let Some(b) =
    ///   get_block(tip+1)`, so every pass ends on a `None` and at the tip those
    ///   outnumber real fetches
    /// - Useful alone = the poll-rate vs block-rate ratio
    pub const SYNC_FETCH_MISSES_TOTAL: &str = "zaino.sync.fetch_misses_total";

    // Write path: B-tree insert+sort, then the device flush. Apart because they
    // saturate for unrelated reasons (working-set vs RAM / the device)
    pub const SYNC_BATCH_WRITE_SECONDS: &str = "zaino.sync.batch_write_seconds";
    pub const SYNC_FSYNC_SECONDS: &str = "zaino.sync.fsync_seconds";
    pub const SYNC_BATCH_BLOCKS: &str = "zaino.sync.batch_blocks";
    pub const SYNC_BATCH_FLUSH_TOTAL: &str = "zaino.sync.batch_flush_total";

    // Deferred txout-set accumulator; runs after the block loop, picks O(range)
    // delta vs from-genesis rebuild. Without `mode` the two share a distribution
    pub const SYNC_ACCUMULATOR_SECONDS: &str = "zaino.sync.accumulator_seconds";
    pub const SYNC_ACCUMULATOR_HEIGHT: &str = "zaino.sync.accumulator_height";

    // Throughput per op class, all [`INGEST_STAGE`]-labelled. Directions apart —
    // only outputs are checkable against the note-commitment trees (`BlockWork`)

    pub const SYNC_TRANSACTIONS_TOTAL: &str = "zaino.sync.transactions_total";
    pub const SYNC_TRANSPARENT_INPUTS_TOTAL: &str = "zaino.sync.transparent_inputs_total";
    pub const SYNC_TRANSPARENT_OUTPUTS_TOTAL: &str = "zaino.sync.transparent_outputs_total";
    pub const SYNC_SAPLING_SPENDS_TOTAL: &str = "zaino.sync.sapling_spends_total";
    pub const SYNC_SAPLING_OUTPUTS_TOTAL: &str = "zaino.sync.sapling_outputs_total";
    pub const SYNC_ORCHARD_ACTIONS_TOTAL: &str = "zaino.sync.orchard_actions_total";
    pub const SYNC_IRONWOOD_ACTIONS_TOTAL: &str = "zaino.sync.ironwood_actions_total";

    pub const FINALISED_EPHEMERAL: &str = "zaino.db.finalised_ephemeral";
    pub const ACCUMULATOR_BUILT_HEIGHT: &str = "zaino.db.accumulator_built_height";
    pub const ACCUMULATOR_REBUILD_ACTIVE: &str = "zaino.db.accumulator_rebuild_active";

    /// Which ingest loop did the work: `finalised` / `migration`.
    ///
    /// - Migration re-reads at the same cost but advances no frontier → folding the two
    ///   inflates the finalised block rate
    pub const INGEST_STAGE: &str = "stage";

    /// Every [`INGEST_STAGE`] value, for counter pre-creation.
    ///
    /// - Never-incremented = absent from a scrape = indistinguishable from unsupported
    /// - Indices match `ingest::IngestStage`, which reads its label from here
    pub const INGEST_STAGES: [&str; 2] = ["finalised", "migration"];

    /// Which read a [`SYNC_FETCH_MISSES_TOTAL`] observation came from: `block` /
    /// `treestate`, so a failing treestate query cannot hide in the block miss rate.
    pub const SOURCE_READ: &str = "read";

    /// Why a read produced no work: `miss` (no such block, normal at the tip) or
    /// `error`. Apart because a rising miss rate is tuning, a rising error rate
    /// is an incident.
    pub const READ_OUTCOME: &str = "outcome";

    /// How a sync iteration ended: `ok` / `error`. Family rate = the heartbeat.
    pub const SYNC_OUTCOME: &str = "outcome";

    /// What ended a write batch: a `bytes` / `blocks` / `interval` cap, or
    /// `target` (reached the sync height and flushed).
    ///
    /// - Says whether `sync_write_batch_size` suits the writer's chain position:
    ///   early blocks hit block & time caps first, at the tip `target` dominates
    pub const BATCH_FLUSH_REASON: &str = "reason";

    /// Every [`BATCH_FLUSH_REASON`] value, for pre-creation.
    pub const BATCH_FLUSH_REASONS: [&str; 4] = ["bytes", "blocks", "interval", "target"];

    /// Accumulator pass: delta vs from-genesis rebuild.
    pub const ACCUMULATOR_MODE: &str = "mode";

    /// Backend serving a routed read: `primary` (DB) / `ephemeral` (passthrough).
    pub const READ_BACKEND: &str = "backend";

    /// Capability surface a routed read requested.
    pub const READ_CAPABILITY: &str = "capability";

    // Storage. used_bytes vs host RAM = write-throughput knee, vs map_size = how
    // full. No LMDB reader slots (`mdb_env_info` = raw FFI, crate forbids unsafe)
    pub const DB_MAP_SIZE_BYTES: &str = "zaino.db.map_size_bytes";
    pub const DB_USED_BYTES: &str = "zaino.db.used_bytes";

    /// How long tip-coherent mempool reads have been frozen.
    ///
    /// - Only mempool metric from this crate (coherence is decided here, against
    ///   the NFS tip); set shape & health live in `zaino_mempool_service`
    pub const MEMPOOL_COHERENCE_FROZEN_SECONDS: &str = "zaino.mempool.coherence_frozen_seconds";

    /// Every histogram emitted above.
    ///
    /// - Unbucketed renders as a summary: rolling-window quantiles, series still
    ///   present — silent, and unfixable here since `zainod` owns the buckets
    /// - `zainod` asserts its table covers this, so a bucketless addition fails
    // `reorg_depth` is absent deliberately: the reorg is observed in
    // `zaino-chain-head-service` now, and it declares its own histogram there
    pub const HISTOGRAM_METRICS: [&str; 8] = [
        SYNC_BLOCK_FETCH_SECONDS,
        SYNC_TREESTATE_FETCH_SECONDS,
        SYNC_BLOCK_ASSEMBLE_SECONDS,
        SYNC_BATCH_WRITE_SECONDS,
        SYNC_FSYNC_SECONDS,
        SYNC_BATCH_BLOCKS,
        SYNC_ACCUMULATOR_SECONDS,
        DB_VALIDATION_SECONDS,
    ];
}

/// Mempool metric names, re-exported for `zainod`'s `describe_*` registrations.
///
/// - `zainod` reaches the mempool only through this crate, but registers every
///   description → re-export keeps names beside their emitter, not retyped
#[cfg(feature = "prometheus")]
pub use zaino_mempool_service::metric_names as mempool_metric_names;

// Zaino's Indexer library frontend.
pub(crate) mod indexer;

pub use indexer::{
    IndexerService, IndexerSubscriber, LightWalletIndexer, LightWalletService, ZcashIndexer,
    ZcashService,
};

pub use indexer::node_backed_indexer::{
    ChainTipSubscriber, NodeBackedIndexerService, NodeBackedIndexerServiceSubscriber,
};

pub mod chain_index;

pub use chain_index::finalised_state::router::FinalisedStateMode;

// Core ChainIndex trait and implementations
pub use chain_index::{
    ChainIndex, ChainIndexRpcExt, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
};
// Source types for ChainIndex backends
pub use chain_index::chain_head::WithChainHeadSource;
pub use chain_index::source::BlockchainSource;
pub use chain_index::source_ports::ChainIndexSourcePorts;
pub use chain_index::validator_source::{ValidatorSource, ZebraValidatorSource};
// Supporting types
pub use chain_index::encoding::*;
// Mempool statistics for `getmempoolinfo`. Currently an on-disk shape in
// `types/db/metadata.rs`; moving it into `zaino-primitives` belongs with the
// persistence rework.
// The non-finalised chain head is `zaino-chain-head`; its runtime is
// `zaino-chain-head-service`. Re-exported here so a consumer wiring a
// ChainIndex does not need to name those crates directly.
pub use chain_index::types::db::metadata::MempoolInfo;
pub use error::{InitError, SyncError};
pub use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
pub use zaino_chain_head_service::MapBackedSnapshot;
// NOTE: Should these be pub at all?
pub use chain_index::types::{
    AddrHistRecord, AddrScript, BlockContext, BlockData, BlockHash, BlockHeaderData, BlockMetadata,
    BlockWithMetadata, ChainWork, ChainWorkError, CommitmentTreeData, CommitmentTreeRoots,
    CommitmentTreeSizes, CompactDifficulty, CompactDifficultyError, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactTxData, Height, IndexedBlock,
    OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList, ScriptType,
    ShardIndex, ShardRoot, TransactionHash, TransparentCompactTx, TransparentTxList, TreeRootData,
    TxInCompact, TxLocation, TxOutCompact, TxidList,
};

#[cfg(feature = "test_dependencies")]
/// allow public access to additional APIs, for testing
pub mod test_dependencies {
    /// Testing export of chain_index
    pub mod chain_index {
        pub use crate::chain_index::*;
    }

    pub use crate::ChainIndexConfig;
}

pub(crate) mod config;

pub use config::{
    ChainIndexConfig, CommonBackendConfig, DirectConnectionConfig, DonationAddress,
    NodeBackedIndexerServiceConfig, ValidatorConnectionType,
};

pub(crate) mod error;

pub use error::{LegacyRpcError, NodeBackedIndexerServiceError};

pub(crate) mod stream;

pub use stream::{
    AddressStream, ChannelStream, CompactBlockStream, CompactTransactionStream,
    RawTransactionStream, StreamObserver, SubtreeRootReplyStream, UtxoReplyStream,
};

pub(crate) mod utils;

pub mod source_caps;
