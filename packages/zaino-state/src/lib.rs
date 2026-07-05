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
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";

    pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
    pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";
    pub const SYNC_LAG_BLOCKS: &str = "zaino.sync.lag_blocks";
    pub const SYNC_ITERATIONS_TOTAL: &str = "zaino.sync.iterations_total";
    pub const SYNC_ITERATION_DURATION_SECONDS: &str = "zaino.sync.iteration_duration_seconds";
    pub const SYNC_ERRORS_TOTAL: &str = "zaino.sync.errors_total";
    pub const SYNC_HAS_REACHED_TIP: &str = "zaino.sync.has_reached_tip";
    pub const SYNC_REACHED_TIP_AT: &str = "zaino.sync.reached_tip_at";
    pub const SYNC_REORG_TOTAL: &str = "zaino.sync.reorg_total";
    pub const SYNC_REORG_DEPTH: &str = "zaino.sync.reorg_depth";
    pub const SYNC_BLOCK_BUILD_SECONDS: &str = "zaino.sync.block_build_seconds";
    pub const SYNC_BLOCK_WRITE_SECONDS: &str = "zaino.sync.block_write_seconds";
    pub const SYNC_TRANSACTIONS_TOTAL: &str = "zaino.sync.transactions_total";
    pub const SYNC_SAPLING_OUTPUTS_TOTAL: &str = "zaino.sync.sapling_outputs_total";
    pub const SYNC_ORCHARD_ACTIONS_TOTAL: &str = "zaino.sync.orchard_actions_total";
    pub const SYNC_LAST_BLOCK_WRITTEN_AT: &str = "zaino.sync.last_block_written_at";

    pub const DB_TIP_HEIGHT: &str = "zaino.db.tip_height";

    pub const MEMPOOL_TRANSACTIONS: &str = "zaino.mempool.transactions";
    pub const MEMPOOL_TIP_CHANGES_TOTAL: &str = "zaino.mempool.tip_changes_total";
}

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

// Core ChainIndex trait and implementations
pub use chain_index::{
    ChainIndex, ChainIndexRpcExt, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
};
// Source types for ChainIndex backends
pub use chain_index::source::{BlockchainSource, State, ValidatorConnector};
// Supporting types
pub use chain_index::encoding::*;
pub use chain_index::mempool::Mempool;
pub use chain_index::non_finalised_state::{
    ChainIndexSnapshot, InitError, NodeConnectionError, NonFinalizedState, SyncError, UpdateError,
};
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

pub use chain_index::mempool::{MempoolKey, MempoolValue};

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

pub use error::NodeBackedIndexerServiceError;

pub(crate) mod status;

pub use status::{AtomicStatus, NamedAtomicStatus, Status, StatusType};

pub(crate) mod stream;

pub use stream::{
    AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
    SubtreeRootReplyStream, UtxoReplyStream,
};

pub(crate) mod broadcast;

pub(crate) mod utils;
