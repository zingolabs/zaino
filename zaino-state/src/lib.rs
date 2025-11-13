//! Zaino's core mempool and chain-fetching Library.
//!
//! Built to use a configurable backend:
//! - FetchService
//!    - Built using the Zcash Json RPC Services for backwards compatibility with Zcashd and other JsonRPC based validators.
//! - StateService
//!    - Built using Zebra's ReadStateService for efficient chain access.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

include!(concat!(env!("OUT_DIR"), "/zebraversion.rs"));

// Zaino's Indexer library frontend.
pub(crate) mod indexer;

pub use indexer::{
    IndexerService, IndexerSubscriber, LightWalletIndexer, LightWalletService, ZcashIndexer,
    ZcashService,
};

pub(crate) mod backends;

#[allow(deprecated)]
pub use backends::{
    fetch::{FetchService, FetchServiceSubscriber},
    state::{StateService, StateServiceSubscriber},
};

// NOTE: This will replace local_cache. Currently WIP.
pub mod chain_index;

// Core ChainIndex trait and implementations
pub use chain_index::{ChainIndex, NodeBackedChainIndex, NodeBackedChainIndexSubscriber};
// Source types for ChainIndex backends
pub use chain_index::source::{BlockchainSource, State, ValidatorConnector};
// Supporting types
pub use chain_index::encoding::*;
pub use chain_index::mempool::Mempool;
pub use chain_index::non_finalised_state::{
    InitError, NodeConnectionError, NonFinalizedState, NonfinalizedBlockCacheSnapshot, SyncError,
    UpdateError,
};
// NOTE: Should these be pub at all?
pub use chain_index::types::{
    AddrHistRecord, AddrScript, BlockData, BlockHash, BlockHeaderData, BlockIndex, BlockMetadata,
    BlockWithMetadata, ChainWork, CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes,
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTxData, Height,
    IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList,
    ScriptType, ShardIndex, ShardRoot, TransactionHash, TransparentCompactTx, TransparentTxList,
    TreeRootData, TxInCompact, TxLocation, TxOutCompact, TxidList,
};

pub(crate) mod local_cache;

pub use chain_index::mempool::{MempoolKey, MempoolValue};

#[cfg(feature = "test_dependencies")]
/// allow public access to additional APIs, for testing
pub mod test_dependencies {
    /// Testing export of chain_index
    pub mod chain_index {
        pub use crate::chain_index::*;
    }
    pub use crate::{config::BlockCacheConfig, local_cache::*};
}

pub(crate) mod config;

#[allow(deprecated)]
pub use config::{
    BackendConfig, BackendType, BlockCacheConfig, FetchServiceConfig, StateServiceConfig,
};

pub(crate) mod error;

#[allow(deprecated)]
pub use error::{FetchServiceError, StateServiceError};

pub(crate) mod status;

pub use status::{AtomicStatus, StatusType};

pub(crate) mod stream;

pub use stream::{
    AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
    SubtreeRootReplyStream, UtxoReplyStream,
};

pub(crate) mod broadcast;

pub(crate) mod utils;
