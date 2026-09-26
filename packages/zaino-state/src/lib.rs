//! Zaino's core mempool and chain-fetching Library.
//!
//! Built to use a configurable backend:
//! - FetchService
//!    - Built using the Zcash Json RPC Services for backwards compatibility with JsonRPC based validators.
//! - StateService
//!    - Built using Zebra's ReadStateService for efficient chain access.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::future::Future;

/// A [`Future`] that is [`Send`] and resolves to `T`.
///
/// Used as `impl SendFut<T>` in trait return position, stating the `Send` bound
/// per method rather than inheriting it implicitly from `async-trait`.
pub trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}

/// Prometheus metric names emitted by this crate, with the `# HELP` `zainod` registers
///
/// - The store's are re-exported (one import site); its tables stay reachable as `store::*`
#[allow(missing_docs)] // the `# HELP` in the table below is the description
pub mod metric_names {
    pub use zaino_chain_store_zainodb::metric_names::{self as store, *};

    // Sync lag = CHAIN_TIP_HEIGHT - SYNC_FINALIZED_HEIGHT, consumer-derived
    pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";
    pub const SYNC_CONSECUTIVE_FAILURES: &str = "zaino.sync.consecutive_failures";
    pub const SYNC_BACKOFF_SECONDS: &str = "zaino.sync.backoff_seconds";
    // Coherence decided against the chain-head tip → emitted from the sync loop, not the mempool
    pub const MEMPOOL_COHERENCE_FROZEN_SECONDS: &str = "zaino.mempool.coherence_frozen_seconds";

    // Shadows the store's glob-imported `GAUGES`
    #[rustfmt::skip]
    pub const GAUGES: &[(&str, &str)] = &[
        (CHAIN_TIP_HEIGHT, "Latest chain tip height reported by the source"),
        (SYNC_CONSECUTIVE_FAILURES, "Consecutive failed sync iterations; 0 when healthy"),
        (SYNC_BACKOFF_SECONDS, "Current sync-loop retry backoff in seconds; 0 when healthy"),
        (MEMPOOL_COHERENCE_FROZEN_SECONDS, "Seconds tip-coherent mempool reads have been frozen; 0 when live"),
    ];
}

/// Mempool metric names; `zainod` reaches the mempool only through this crate
pub use zaino_mempool_service::metric_names as mempool_metric_names;

// Zaino's Indexer library frontend.
pub(crate) mod indexer;

pub use indexer::{
    IndexedTipIndexer, IndexerService, IndexerSubscriber, LightWalletIndexer, LightWalletService,
    ZcashIndexer, ZcashService,
};
pub use stream::IndexedTipStream;

pub use indexer::node_backed_indexer::{
    ChainTipSubscriber, NodeBackedIndexerService, NodeBackedIndexerServiceSubscriber,
};

pub mod chain_index;

pub use zaino_chain_store_zainodb::store::FinalisedStateMode;

// Core ChainIndex trait and implementations
pub use chain_index::{
    ChainIndex, ChainIndexRpcExt, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
};
// Source types for ChainIndex backends
pub use chain_index::chain_head::WithChainHeadSource;
pub use chain_index::chain_store::WithChainStoreSource;
pub use chain_index::source::BlockchainSource;
pub use chain_index::source_ports::ChainIndexSourcePorts;
pub use chain_index::validator_source::{ValidatorSource, ZebraValidatorSource};
// Supporting types
// Mempool statistics for `getmempoolinfo`, now `zaino-primitives` vocabulary.
// Re-exported so a consumer wiring a ChainIndex need not name that crate.
// The non-finalised chain head is `zaino-chain-head`; its runtime is
// `zaino-chain-head-service`. Re-exported here so a consumer wiring a
// ChainIndex does not need to name those crates directly.
pub use error::{InitError, SyncError};
pub use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
pub use zaino_chain_head_service::MapBackedSnapshot;
pub use zaino_primitives::types::MempoolInfo;

/// The finalised store's on-disk types, for this crate's own use only.
///
/// These were `pub`, with a note asking whether they should be. They should
/// not: they are `zaino-chain-store-zainodb`'s persisted shapes, and a consumer
/// written against them is written against one backend's disk layout. A
/// consumer that genuinely needs them — the live-test legacy parser, which
/// rebuilds a block independently and compares — names that crate directly.
///
/// `pub(crate)` rather than deleted because this crate still reads both halves
/// of the chain through `IndexedBlock`. The re-export goes when it stops.
///
/// `TxOutCompact` has already gone: the finalised reads now come back as
/// `zaino_chain_store::StoredTxOut` through the ports, so the one place that
/// held a stored output — the cross-seam UTXO fold — folds domain outputs
/// instead. The rest of this list shrinks the same way.
pub(crate) use chain_index::types::{BlockHash, Height, IndexedBlock, Outpoint, TransactionHash};

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
    AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
    SubtreeRootReplyStream, UtxoReplyStream,
};

pub(crate) mod utils;

pub mod source_caps;
