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

#[cfg(feature = "prometheus")]
pub mod metric_names {

    pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";
    pub const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
    pub const SYNC_FETCHED_HEIGHT: &str = "zaino.sync.fetched_height";
    pub const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";

    pub const SYNC_REORG_DEPTH: &str = "zaino.sync.reorg_depth";

    pub const SYNC_BLOCK_BUILD_SECONDS: &str = "zaino.sync.block_build_seconds";
    pub const SYNC_BLOCK_FETCH_SECONDS: &str = "zaino.sync.block_fetch_seconds";
    pub const SYNC_BATCH_WRITE_SECONDS: &str = "zaino.sync.batch_write_seconds";

    pub const SYNC_TRANSACTIONS_TOTAL: &str = "zaino.sync.transactions_total";
    pub const SYNC_TRANSPARENT_OPS_TOTAL: &str = "zaino.sync.transparent_ops_total";
    pub const SYNC_SAPLING_OPS_TOTAL: &str = "zaino.sync.sapling_ops_total";
    pub const SYNC_ORCHARD_ACTIONS_TOTAL: &str = "zaino.sync.orchard_actions_total";
    pub const SYNC_IRONWOOD_ACTIONS_TOTAL: &str = "zaino.sync.ironwood_actions_total";

    pub const DB_MAP_SIZE_BYTES: &str = "zaino.db.map_size_bytes";
    pub const DB_USED_BYTES: &str = "zaino.db.used_bytes";
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
