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

pub use backends::{
    fetch::{FetchService, FetchServiceSubscriber},
    state::{StateService, StateServiceSubscriber},
};

// NOTE: This will replace local_cache. Currently WIP.
pub(crate) mod chain_index;

// NOTE: Should these be pub at all?
pub use chain_index::types::{
    AddrHistRecord, AddrScript, BlockData, BlockHeaderData, BlockIndex, ChainBlock, ChainWork,
    CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactTxData, Hash, Height, Index,
    OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList, ScriptType,
    ShardRoot, TransparentCompactTx, TransparentTxList, TxInCompact, TxIndex, TxOutCompact,
    TxidList,
};

pub use chain_index::encoding::*;

pub(crate) mod local_cache;

pub use local_cache::mempool::{MempoolKey, MempoolValue};

#[cfg(feature = "bench")]
/// allow public access to additional APIs, for testing
pub mod bench {
    /// Testing export of chain_index
    pub mod chain_index {
        pub use crate::chain_index::*;
    }
    pub use crate::{config::BlockCacheConfig, local_cache::*};
}

pub(crate) mod config;

pub use config::{BackendConfig, BackendType, FetchServiceConfig, StateServiceConfig};

pub(crate) mod error;

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
