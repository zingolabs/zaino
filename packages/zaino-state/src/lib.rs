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
/// Written as `impl SendFut<T>` in trait method return positions so the `Send`
/// bound the `async-trait` macro previously supplied implicitly is stated
/// explicitly per method. See `docs/adr/0002-native-afit-over-async-trait.md`.
pub trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}

#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    //! Prometheus metric names, and the single source of truth shared with
    //! `zainod`'s `describe_*` registrations, which carry the descriptions.
    //!
    //! Each name is defined once, in the crate that emits it.
    //!
    //! The finalised store's write-path metrics are emitted from
    //! `zaino-chain-store-zainodb` and so are defined there and re-exported
    //! here. Restating them would put the live string and the pinned string in
    //! different crates: a rename where the metric is emitted would break every
    //! dashboard built on it while the pin test — which reads this module —
    //! went on comparing a copy nothing publishes. A re-export cannot drift.
    //!
    //! This module remains the single import site, so `zainod`'s `describe_*`
    //! registrations and the bench harness are unaffected by where a given
    //! name lives.
    pub use zaino_chain_store_zainodb::metric_names::*;

    pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";

    pub const SYNC_LAG_BLOCKS: &str = "zaino.sync.lag_blocks";
    pub const SYNC_ITERATIONS_TOTAL: &str = "zaino.sync.iterations_total";
    pub const SYNC_ITERATION_DURATION_SECONDS: &str = "zaino.sync.iteration_duration_seconds";
    pub const SYNC_ERRORS_TOTAL: &str = "zaino.sync.errors_total";
    pub const SYNC_HAS_REACHED_TIP: &str = "zaino.sync.has_reached_tip";
    pub const SYNC_REACHED_TIP_AT: &str = "zaino.sync.reached_tip_at";

    pub const MEMPOOL_COHERENCE_FROZEN_SECONDS: &str = "zaino.mempool.coherence_frozen_seconds";
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
// Zaino's versioned encoding now lives in `zaino-encoding`, so a storage
// backend and a wire codec can share it. Re-exported unchanged: every consumer
// of these names is mid-migration and should not have to move at the same time.
pub use zaino_encoding::*;
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
