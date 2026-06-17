//! Holds Zaino's local chain index.
//!
//! Components:
//! - Mempool: Holds mempool transactions
//! - NonFinalisedState: Holds block data for the top 100 blocks of all chains.
//! - FinalisedState: Holds block data for the remainder of the best chain.
//!
//! - Chain: Holds chain / block structs used internally by the ChainIndex.
//!   - Holds fields required to:
//!     - a. Serve CompactBlock data dirctly.
//!     - b. Build trasparent tx indexes efficiently
//!   - NOTE: Full transaction and block data is served from the backend finalizer.

use crate::chain_index::non_finalised_state::ChainIndexSnapshot;
use crate::chain_index::source::GetTransactionLocation;
use crate::chain_index::types::db::metadata::MempoolInfo;
use crate::chain_index::types::helpers::{BlockMetadata, BlockWithMetadata, TreeRootData};
use crate::chain_index::types::BlockIndex;
use crate::chain_index::types::{BestChainLocation, NonBestChainLocation};
use crate::error::{ChainIndexError, ChainIndexErrorKind, FinalisedStateError};
use crate::status::Status;
use crate::{
    ChainWork, CompactBlockStream, NamedAtomicStatus, NonFinalizedState, StatusType, SyncError,
    TxOutCompact,
};
use crate::{IndexedBlock, Outpoint, TransactionHash};
use std::collections::HashSet;
use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwapOption;
use futures::{FutureExt, Stream};
use hex::FromHex as _;
use non_finalised_state::NonfinalizedBlockCacheSnapshot;
use source::{BlockchainSource, ValidatorConnector};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};
use zaino_fetch::jsonrpsee::response::{
    address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
    chain_tips::{ChainTip, ChainTipStatus, GetChainTipsResponse},
    EmptyTxOutSetInfo, GetTxOutSetInfo, GetTxOutSetInfoResponse,
};
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};
use zebra_chain::parameters::ConsensusBranchId;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_chain::serialization::ZcashSerialize;
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::{AddressBalance, GetAddressUtxos},
};
use zebra_state::HashOrHeight;

pub mod encoding;
/// All state below [`NON_FINALIZED_DEPTH`] blocks of the best-known chain tip.
pub mod finalised_state;
/// State in the mempool, not yet on-chain
pub mod mempool;
/// State within [`NON_FINALIZED_DEPTH`] blocks of the best-known chain tip;
/// stored separately as it may be reorged.
pub mod non_finalised_state;
/// BlockchainSource
pub mod source;
/// Common types used by the rest of this module
pub mod types;

#[cfg(test)]
mod tests;

/// Distance (in blocks) between the best-known chain tip and the
/// highest block that zaino treats as part of the finalized DB.
///
/// Sourced from Zebra's protocol-derived reorg bound. The `+ 1`
/// preserves the original literal-`100` behavior; deriving the
/// depth from an explicit wider-consensus reference is tracked in
/// zingolabs/zaino#1130.
pub(crate) const NON_FINALIZED_DEPTH: u32 = zebra_state::MAX_BLOCK_REORG_HEIGHT + 1;

/// Lower bound on zaino's finalized-DB tip, derived from the current
/// best-known chain tip.
///
/// After a chain-shortening reorg this floor can move backwards while
/// the on-disk `finalized_height` does not — finalized blocks are
/// never evicted. Callers comparing this floor against
/// `finalized_height` should account for the asymmetry
/// (see zingolabs/zaino#1128).
pub(crate) fn finalized_height_floor(chain_tip: u32) -> crate::Height {
    crate::Height(chain_tip.saturating_sub(NON_FINALIZED_DEPTH))
}

/// Builds a zcashd-compatible `getchaintips` response from the local non-finalized snapshot.
///
/// zcashd enumerates block-tree leaves, always includes the active tip, and reports
/// inactive fully-known branches as `valid-fork`. Zaino's non-finalized cache stores
/// full blocks, not headers-only or invalid candidates, so those are the only statuses
/// this conversion can currently emit.
pub(crate) fn chain_tips_from_nonfinalized_snapshot(
    snapshot: &NonfinalizedBlockCacheSnapshot,
) -> GetChainTipsResponse {
    let parent_hashes = snapshot
        .blocks
        .values()
        .map(|block| *block.context.parent_hash())
        .collect::<HashSet<_>>();

    let mut tip_hashes = snapshot
        .blocks
        .keys()
        .filter(|hash| !parent_hashes.contains(hash))
        .copied()
        .collect::<HashSet<_>>();
    tip_hashes.insert(snapshot.best_tip.hash);

    let mut tips = tip_hashes
        .into_iter()
        .filter_map(|hash| snapshot.blocks.get(&hash))
        .map(|block| {
            let is_active_tip = block.hash() == &snapshot.best_tip.hash;
            let status = if is_active_tip {
                ChainTipStatus::Active
            } else {
                ChainTipStatus::ValidFork
            };
            let branchlen = if is_active_tip {
                0
            } else {
                branch_len_to_active_chain(snapshot, block)
            };

            ChainTip::new(
                u32::from(block.height()),
                block.hash().to_rpc_hex(),
                branchlen,
                status,
            )
        })
        .collect::<Vec<_>>();

    tips.sort_by(|left, right| {
        right
            .height
            .cmp(&left.height)
            .then_with(|| left.hash.cmp(&right.hash))
    });
    tips
}

fn branch_len_to_active_chain(
    snapshot: &NonfinalizedBlockCacheSnapshot,
    block: &IndexedBlock,
) -> u32 {
    let mut branch_len = 0;
    let mut current = block;

    loop {
        if snapshot.heights_to_hashes.get(&current.height()) == Some(current.hash()) {
            return branch_len;
        }

        branch_len += 1;

        let parent_hash = current.context.parent_hash();
        let Some(parent) = snapshot.blocks.get(parent_hash) else {
            return branch_len;
        };
        current = parent;
    }
}

/// The interface to the chain index.
///
/// `ChainIndex` provides a unified interface for querying blockchain data from different
/// backend sources. It combines access to both finalized state (older than 100 blocks) and
/// non-finalized state (recent blocks that may still be reorganized).
///
/// # Implementation
///
/// The primary implementation is [`NodeBackedChainIndex`], which can be backed by either:
/// - Direct read access to a zebrad database via `ReadStateService` (preferred)
/// - A JSON-RPC connection to a validator node (zcashd, zebrad, or another zainod)
///
/// # Example with ReadStateService (Preferred)
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::{ChainIndex, NodeBackedChainIndex, ValidatorConnector, BlockCacheConfig};
/// use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
/// use zebra_state::{ReadStateService, Config as ZebraConfig};
/// use std::path::PathBuf;
///
/// // Create a ReadStateService for direct database access
/// let zebra_config = ZebraConfig::default();
/// let read_state_service = ReadStateService::new(&zebra_config).await?;
///
/// // Create a JSON-RPC connector for mempool access (temporary requirement)
/// let mempool_connector = JsonRpSeeConnector::new_from_config_parts(
///     false, // no cookie auth
///     "127.0.0.1:8232".parse()?,
///     "user".to_string(),
///     "password".to_string(),
///     None,  // no cookie path
/// ).await?;
///
/// // Create the State source combining both services
/// let source = ValidatorConnector::State(zaino_state::chain_index::source::State {
///     read_state_service,
///     mempool_fetcher: mempool_connector,
/// });
///
/// // Configure the block cache
/// let config = BlockCacheConfig::new(
///     None,  // map capacity
///     None,  // shard amount
///     1,     // db version
///     PathBuf::from("/path/to/cache"),
///     None,  // db size
///     zebra_chain::parameters::Network::Mainnet,
///     false, // sync enabled
///     false, // db enabled
/// );
///
/// // Create the chain index and get a subscriber for queries
/// let chain_index = NodeBackedChainIndex::new(source, config).await?;
/// let subscriber = chain_index.subscriber().await;
///
/// // Take a snapshot for consistent queries
/// let snapshot = subscriber.snapshot_nonfinalized_state();
///
/// // Query blocks in a range using the subscriber
/// if let Some(stream) = subscriber.get_block_range(
///     &snapshot,
///     zaino_state::Height(100000),
///     Some(zaino_state::Height(100010))
/// ) {
///     // Process the block stream...
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Example with JSON-RPC Only (Fallback)
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::{ChainIndex, NodeBackedChainIndex, ValidatorConnector, BlockCacheConfig};
/// use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
/// use std::path::PathBuf;
///
/// // Create a JSON-RPC connector to your validator node
/// let connector = JsonRpSeeConnector::new_from_config_parts(
///     false, // no cookie auth
///     "127.0.0.1:8232".parse()?,
///     "user".to_string(),
///     "password".to_string(),
///     None,  // no cookie path
/// ).await?;
///
/// // Wrap the connector for use with ChainIndex
/// let source = ValidatorConnector::Fetch(connector);
///
/// // Configure the block cache (same as above)
/// let config = BlockCacheConfig::new(
///     None,  // map capacity
///     None,  // shard amount
///     1,     // db version
///     PathBuf::from("/path/to/cache"),
///     None,  // db size
///     zebra_chain::parameters::Network::Mainnet,
///     false, // sync enabled
///     false, // db enabled
/// );
///
/// // Create the chain index and get a subscriber for queries
/// let chain_index = NodeBackedChainIndex::new(source, config).await?;
/// let subscriber = chain_index.subscriber().await;
///
/// // Use the subscriber to access ChainIndex trait methods
/// let snapshot = subscriber.snapshot_nonfinalized_state();
/// # Ok(())
/// # }
/// ```
///
/// # Migrating from FetchService or StateService
///
/// If you were previously using `FetchService::spawn()` or `StateService::spawn()`:
/// 1. Extract the relevant fields from your service config into a `BlockCacheConfig`
/// 2. Create the appropriate `ValidatorConnector` variant (State or Fetch)
/// 3. Call `NodeBackedChainIndex::new(source, config).await`
///
/// When a call asks for info (e.g. a block), Zaino selects sources in this order:
#[doc = simple_mermaid::mermaid!("chain_index_passthrough.mmd")]
pub trait ChainIndex {
    /// A snapshot of the nonfinalized state, needed for atomic access
    type Snapshot: NonFinalizedSnapshot;

    /// How it can fail
    type Error;

    // ********** Utility methods **********

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Snapshot, Self::Error>>;

    // ********** Block methods **********

    /// Returns Some(Height) for the given block hash *if* it is currently in the best chain.
    ///
    /// Returns None if the specified block is not in the best chain or is not found.
    fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<types::Height>, Self::Error>>;

    /// Returns Some(BlockHash) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::Height,
    ) -> impl std::future::Future<Output = Result<Option<types::BlockHash>, Self::Error>>;

    /// Returns Some(IndexedBlock) for the given block hash.
    ///
    /// Returns None if the specified block is not found.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Returns Some(IndexedBlock) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified end height
    /// is greater than the snapshot's tip
    #[allow(clippy::type_complexity)]
    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: Option<types::Height>,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;

    /// Returns the *compact* block for the given height.
    ///
    /// Returns `None` if the specified `height` is greater than the snapshot's tip.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    #[allow(clippy::type_complexity)]
    fn get_compact_block(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> impl std::future::Future<
        Output = Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error>,
    >;

    /// Streams *compact* blocks for an inclusive height range.
    ///
    /// Returns `None` if the requested range is entirely above the snapshot's tip.
    ///
    /// - The stream covers `[start_height, end_height]` (inclusive).
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    #[allow(clippy::type_complexity)]
    fn get_compact_block_stream(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> impl std::future::Future<Output = Result<Option<CompactBlockStream>, Self::Error>>;

    // ********** Transaction methods **********

    /// given a transaction id, returns the transaction, along with
    /// its consensus branch ID if available
    #[allow(clippy::type_complexity)]
    fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<Output = Result<Option<(Vec<u8>, Option<u32>)>, Self::Error>>;

    /// Given a transaction ID, returns all known hashes and heights of blocks
    /// containing that transaction.
    ///
    /// Also returns if the transaction is in the mempool (and whether that mempool is
    /// in-sync with the provided snapshot)
    #[allow(clippy::type_complexity)]
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), Self::Error>,
    >;

    /// Returns all txids currently in the mempool.
    fn get_mempool_txids(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<types::TransactionHash>, Self::Error>>;

    /// Returns all transactions currently in the mempool, filtered by `exclude_list`.
    ///
    /// The `exclude_list` may contain shortened transaction ID hex prefixes (client-endian).
    fn get_mempool_transactions(
        &self,
        exclude_list: Vec<String>,
    ) -> impl std::future::Future<Output = Result<Vec<Vec<u8>>, Self::Error>>;

    /// Returns a stream of mempool transactions, ending the stream when the chain tip block hash
    /// changes (a new block is mined or a reorg occurs).
    ///
    /// If a snapshot is given and the chain tip has changed from the given spanshot, returns None.
    #[allow(clippy::type_complexity)]
    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;

    // ********** Chain methods **********

    /// Get the tip of the best chain, according to the snapshot
    fn best_chaintip(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
    ) -> impl std::future::Future<Output = Result<BlockIndex, Self::Error>>;

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<(types::BlockHash, types::Height)>, Self::Error>>;

    /// Returns the block commitment tree data by hash
    #[allow(clippy::type_complexity)]
    fn get_treestate(
        &self,
        // snapshot: &Self::Snapshot,
        // currently not implemented internally, fetches data from validator.
        //
        // NOTE: Should this check blockhash exists in snapshot and db before proxying call?
        hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<(Option<Vec<u8>>, Option<Vec<u8>>), Self::Error>>;

    /// Returns the subtree roots
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> impl std::future::Future<Output = Result<Vec<([u8; 32], u32)>, Self::Error>>;

    // ********** Transparent address history methods **********

    /// Returns all changes for the given transparent addresses.
    fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> impl std::future::Future<Output = Result<GetAddressDeltasResponse, Self::Error>>;

    /// Returns the total transparent balance for the given addresses.
    fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl std::future::Future<Output = Result<AddressBalance, Self::Error>>;

    /// Returns the transaction ids made by the given transparent addresses.
    fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> impl std::future::Future<Output = Result<Vec<types::TransactionHash>, Self::Error>>;

    /// Returns all unspent transparent outputs for the given addresses.
    fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl std::future::Future<Output = Result<Vec<GetAddressUtxos>, Self::Error>>;

    // ********** Metadata methods **********

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    fn get_mempool_info(&self) -> impl std::future::Future<Output = MempoolInfo>;

    /// Returns the full `gettxoutsetinfo` response, folding the non-finalised state on top of
    /// the finalised txout-set accumulator.
    ///
    /// Returns [`GetTxOutSetInfoResponse::Empty`] while the indexer is still syncing the
    /// finalised state (the accumulator's spent-index invariants are not yet established).
    fn get_tx_out_set_info(
        &self,
    ) -> impl std::future::Future<Output = Result<GetTxOutSetInfoResponse, Self::Error>>;
}

/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once.
///
/// This is the primary implementation backing [`ChainIndex`] and replaces the functionality
/// previously provided by `FetchService` and `StateService`. It can be backed by either:
/// - A zebra `ReadStateService` for direct database access (preferred for performance)
/// - A JSON-RPC connection to any validator node (zcashd, zebrad, or another zainod)
///
/// To use the [`ChainIndex`] trait methods, call [`subscriber()`](NodeBackedChainIndex::subscriber)
/// to get a [`NodeBackedChainIndexSubscriber`] which implements the trait.
///
/// # Construction
///
/// Use [`NodeBackedChainIndex::new()`] with:
/// - A [`ValidatorConnector`] source (State variant preferred, Fetch as fallback)
/// - A [`crate::config::BlockCacheConfig`] containing cache and database settings
///
/// # Example with StateService (Preferred)
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::{NodeBackedChainIndex, ValidatorConnector, BlockCacheConfig};
/// use zaino_state::chain_index::source::State;
/// use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
/// use zebra_state::{ReadStateService, Config as ZebraConfig};
/// use std::path::PathBuf;
///
/// // Create ReadStateService for direct database access
/// let zebra_config = ZebraConfig::default();
/// let read_state_service = ReadStateService::new(&zebra_config).await?;
///
/// // Temporary: Create JSON-RPC connector for mempool access
/// let mempool_connector = JsonRpSeeConnector::new_from_config_parts(
///     false,
///     "127.0.0.1:8232".parse()?,
///     "user".to_string(),
///     "password".to_string(),
///     None,
/// ).await?;
///
/// let source = ValidatorConnector::State(State {
///     read_state_service,
///     mempool_fetcher: mempool_connector,
/// });
///
/// // Configure the cache (extract these from your previous StateServiceConfig)
/// let config = BlockCacheConfig {
///     map_capacity: Some(1000),
///     map_shard_amount: Some(16),
///     db_version: 1,
///     db_path: PathBuf::from("/path/to/cache"),
///     db_size: Some(10), // GB
///     network: zebra_chain::parameters::Network::Mainnet,
///     no_sync: false,
///     no_db: false,
/// };
///
/// let chain_index = NodeBackedChainIndex::new(source, config).await?;
/// let subscriber = chain_index.subscriber().await;
///
/// // Use the subscriber to access ChainIndex trait methods
/// let snapshot = subscriber.snapshot_nonfinalized_state();
/// # Ok(())
/// # }
/// ```
///
/// # Example with JSON-RPC Only (Fallback)
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::{NodeBackedChainIndex, ValidatorConnector, BlockCacheConfig};
/// use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
/// use std::path::PathBuf;
///
/// // For JSON-RPC backend (replaces FetchService::spawn)
/// let connector = JsonRpSeeConnector::new_from_config_parts(
///     false,
///     "127.0.0.1:8232".parse()?,
///     "user".to_string(),
///     "password".to_string(),
///     None,
/// ).await?;
/// let source = ValidatorConnector::Fetch(connector);
///
/// // Configure the cache (extract these from your previous FetchServiceConfig)
/// let config = BlockCacheConfig {
///     map_capacity: Some(1000),
///     map_shard_amount: Some(16),
///     db_version: 1,
///     db_path: PathBuf::from("/path/to/cache"),
///     db_size: Some(10), // GB
///     network: zebra_chain::parameters::Network::Mainnet,
///     no_sync: false,
///     no_db: false,
/// };
///
/// let chain_index = NodeBackedChainIndex::new(source, config).await?;
/// let subscriber = chain_index.subscriber().await;
///
/// // Use the subscriber to access ChainIndex trait methods
/// # Ok(())
/// # }
/// ```
///
/// # Migration from StateService/FetchService
///
/// If migrating from `StateService::spawn(config)`:
/// 1. Create a `ReadStateService` and temporary JSON-RPC connector for mempool
/// 2. Convert config to `BlockCacheConfig` (or use `From` impl)
/// 3. Call `NodeBackedChainIndex::new(ValidatorConnector::State(...), block_config)`
///
/// If migrating from `FetchService::spawn(config)`:
/// 1. Create a `JsonRpSeeConnector` using the RPC fields from your `FetchServiceConfig`
/// 2. Convert remaining config fields to `BlockCacheConfig` (or use `From` impl)
/// 3. Call `NodeBackedChainIndex::new(ValidatorConnector::Fetch(connector), block_config)`
///
/// # Current Features
///
/// - Full mempool support including streaming and filtering
/// - Unified access to finalized and non-finalized blockchain state
/// - Automatic synchronization between state layers
/// - Snapshot-based consistency for queries
#[derive(Debug)]
pub struct NodeBackedChainIndex<Source: BlockchainSource = ValidatorConnector> {
    #[allow(dead_code)]
    mempool: std::sync::Arc<mempool::Mempool<Source>>,
    non_finalized_state: Arc<ArcSwapOption<crate::NonFinalizedState<Source>>>,
    finalized_db: std::sync::Arc<finalised_state::ZainoDB>,
    sync_loop_handle: Option<tokio::task::JoinHandle<Result<(), SyncError>>>,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: Source,
    sync_timings: SyncTimings,
    /// Signals the sync worker to exit cooperatively. `shutdown()` fires
    /// `cancel_token.cancel()` *before* tearing down `finalized_db`, so the
    /// worker wakes from any in-flight `tokio::time::sleep` and returns
    /// `Ok(())` instead of cycling through the failure-escalation ladder
    /// once `fs.*` calls start failing. Closes the race tracked in #1098.
    cancel_token: CancellationToken,
}

/// Timing parameters for the ChainIndex sync loop.
///
/// [`SyncTimings::default()`] produces production values (500 ms inter-iteration
/// sleep, 250 ms initial backoff doubling up to 8 s, 10 consecutive failures
/// before escalating to [`StatusType::CriticalError`] — ~40 s total window).
/// [`SyncTimings::fast()`] shrinks each duration by 10× so backoff-dependent
/// unit tests finish in ~4 s instead of ~40 s.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SyncTimings {
    pub(crate) interval: Duration,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
    pub(crate) max_consecutive_failures: u32,
}

impl Default for SyncTimings {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
            max_consecutive_failures: 10,
        }
    }
}

#[cfg(test)]
impl SyncTimings {
    /// 10× faster than [`Self::default`] — test-only.
    pub(crate) const fn fast() -> Self {
        Self {
            interval: Duration::from_millis(50),
            initial_backoff: Duration::from_millis(25),
            max_backoff: Duration::from_millis(800),
            max_consecutive_failures: 10,
        }
    }

    /// Upper bound on the cumulative sleep the sync loop performs before
    /// escalating to [`StatusType::CriticalError`] under persistent failure.
    ///
    /// Sums backoff delays for failures `1..max_consecutive_failures` — the
    /// final failure sets CriticalError without sleeping.
    pub(crate) fn max_backoff_window(&self) -> Duration {
        let mut total = Duration::ZERO;
        let mut current = self.initial_backoff;
        for _ in 0..self.max_consecutive_failures.saturating_sub(1) {
            total += current;
            current = (current * 2).min(self.max_backoff);
        }
        total
    }
}

impl<Source: BlockchainSource> NodeBackedChainIndex<Source> {
    /// Creates a new chainindex from a connection to a validator
    /// Currently this is a ReadStateService or JsonRpSeeConnector
    pub async fn new(
        source: Source,
        config: crate::config::BlockCacheConfig,
    ) -> Result<Self, crate::InitError> {
        Self::new_with_sync_timings(source, config, SyncTimings::default()).await
    }

    /// Like [`Self::new`] but overrides the sync-loop timings. Intended for
    /// tests that exercise the backoff path and need a faster schedule.
    pub(crate) async fn new_with_sync_timings(
        source: Source,
        config: crate::config::BlockCacheConfig,
        sync_timings: SyncTimings,
    ) -> Result<Self, crate::InitError> {
        use futures::TryFutureExt as _;

        let finalized_db =
            Arc::new(finalised_state::ZainoDB::spawn(config.clone(), source.clone()).await?);
        let mempool_state = mempool::Mempool::spawn(source.clone(), None)
            .map_err(crate::InitError::MempoolInitialzationError)
            .await?;

        let mut chain_index = Self {
            mempool: std::sync::Arc::new(mempool_state),
            non_finalized_state: Arc::new(ArcSwapOption::empty()),
            finalized_db,
            sync_loop_handle: None,
            status: NamedAtomicStatus::new("ChainIndex", StatusType::Spawning),
            network: config.network.to_zebra_network(),
            source,
            sync_timings,
            cancel_token: CancellationToken::new(),
        };
        chain_index.sync_loop_handle = Some(chain_index.start_sync_loop());

        Ok(chain_index)
    }

    /// Creates a [`NodeBackedChainIndexSubscriber`] from self,
    /// a clone-safe, drop-safe, read-only view onto the running indexer.
    pub fn subscriber(&self) -> NodeBackedChainIndexSubscriber<Source> {
        NodeBackedChainIndexSubscriber {
            mempool: self.mempool.subscriber(),
            non_finalized_state: self.non_finalized_state.clone(),
            finalized_state: self.finalized_db.to_reader(),
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }

    /// Shut down the sync process, for a cleaner drop.
    /// An error indicates a failure to cleanly shutdown. Dropping the
    /// chain index should still stop everything.
    ///
    /// Order matters: `cancel_token.cancel()` runs *before* `fs.shutdown()`
    /// so the sync worker wakes from its post-iter sleep and exits via the
    /// cancellation arm. If we tore down `fs` first, the worker's next
    /// `fs.sync_to_height` call would fail, the failure path would
    /// `tokio::time::sleep(current_backoff)`, and only the cancellation
    /// arm on *that* sleep would release the worker — which is exactly
    /// the design we have. Cancelling first just removes the wasted
    /// failure-path round trip.
    pub async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.cancel_token.cancel();
        self.status.store(StatusType::Closing);
        self.finalized_db.shutdown().await?;
        self.mempool.close();
        Ok(())
    }

    /// Displays the status of the chain_index
    pub fn status(&self) -> StatusType {
        let finalized_status = self.finalized_db.status();
        let mempool_status = self.mempool.status();
        let combined_status = self
            .status
            .load()
            .combine(finalized_status)
            .combine(mempool_status);
        self.status.store(combined_status);
        combined_status
    }

    #[instrument(name = "ChainIndex::start_sync_loop", skip(self))]
    pub(super) fn start_sync_loop(&self) -> tokio::task::JoinHandle<Result<(), SyncError>> {
        info!("Starting ChainIndex sync loop");
        let nfs = self.non_finalized_state.clone();
        let fs = self.finalized_db.clone();
        let status = self.status.clone();
        let source = self.source.clone();
        let network = self.network.clone();
        let timings = self.sync_timings;
        let cancel_token = self.cancel_token.clone();

        tokio::task::spawn(async move {
            let status = status.clone();
            let source = source.clone();
            // Subscribe once to source-change notifications (mockchain
            // sources fire on `mine_blocks`; real validators return None
            // and the worker falls back to its interval timer).
            let mut change_rx = source.subscribe_to_blocks_received();
            let mut consecutive_failures: u32 = 0;
            let mut current_backoff = timings.initial_backoff;

            loop {
                let source = source.clone();
                let network = network.clone();
                if cancel_token.is_cancelled() {
                    return Ok(());
                }

                status.store(StatusType::Syncing);

                // Race the iter body against cancellation: any await inside
                // — `source.get_best_block_height`, `fs.sync_to_height`,
                // `non_finalized_state.sync` — is a checkpoint that can
                // short-circuit to `Ok(())` when `cancel_token.cancel()`
                // fires. All in-flight ops drop cleanly (LMDB writes are
                // per-block atomic, ArcSwap CAS is single-tick, local
                // `Vec`s/`HashMap`s are scoped to the dropped future). Lets
                // tests drop the indexer without calling `shutdown()` and
                // still have the worker exit promptly via the `Drop` impl
                // below.
                let sync_result: Result<(), SyncError> = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => return Ok(()),
                    r = async {
                    fn source_error(error: impl std::error::Error + Send + 'static) -> SyncError {
                        SyncError::ErrorFromSource(Box::new(error))
                    }

                    let chain_height = source
                        .clone()
                        .get_best_block_height()
                        .await
                        .map_err(source_error)?
                        .ok_or_else(|| {
                            source_error(std::io::Error::other(
                                "node returned no best block height",
                            ))
                        })?;
                    let finalised_height = finalized_height_floor(chain_height.0);

                    fs.sync_to_height(finalised_height, &source)
                        .await
                        .map_err(source_error)?;

                    let intermediate_nfs_for_scoping = nfs.load();
                    let non_finalized_state = match *intermediate_nfs_for_scoping {
                        Some(ref nfs) => nfs,
                        None => {
                            nfs.store(Some(Arc::new(
                                NonFinalizedState::initialize(
                                    source,
                                    network,
                                    fs.to_reader()
                                        .get_chain_block_by_height(finalised_height)
                                        .await
                                        .expect("todo"),
                                )
                                .await
                                .expect("todo"),
                            )));
                            &nfs.load_full().expect("just set to Some")
                        }
                    };

                    // Sync nfs to the iter-committed `chain_height`, trimming
                    // blocks to finalized tip. Passing `chain_height` rather
                    // than letting NFS extend until `get_block` returns None
                    // bounds the iter against mid-iter source advances (#1126).
                    non_finalized_state
                        .sync(fs.clone(), chain_height.into())
                        .await?;
                    std::mem::drop(intermediate_nfs_for_scoping);

                    Ok(())
                    } => r,
                };

                match sync_result {
                    Ok(()) => {
                        consecutive_failures = 0;
                        current_backoff = timings.initial_backoff;
                        status.store(StatusType::Ready);
                        // Race the post-success wait against cancellation
                        // and a source-change notification. `shutdown()`'s
                        // `cancel_token.cancel()` releases this immediately
                        // so the next top-of-loop check exits the worker;
                        // a source change wakes the worker before the full
                        // `timings.interval` elapses, so newly-mined
                        // blocks land in the next iter without waiting on
                        // the timer.
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => return Ok(()),
                            _ = source::wait_or_source_change(
                                change_rx.as_mut(),
                                timings.interval,
                            ) => {}
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= timings.max_consecutive_failures {
                            tracing::error!(
                                "Sync loop failed {consecutive_failures} consecutive times, \
                                 giving up: {e:?}"
                            );
                            status.store(StatusType::CriticalError);
                            return Err(e);
                        }
                        tracing::warn!(
                            "Sync loop iteration failed ({consecutive_failures}/{}), \
                             retrying in {current_backoff:?}: {e:?}",
                            timings.max_consecutive_failures
                        );
                        status.store(StatusType::RecoverableError);
                        // Race the failure-path backoff sleep against
                        // cancellation. Without this, `shutdown()` after
                        // `fs.shutdown()` would force the worker through
                        // the full ~40 s `max_consecutive_failures`
                        // backoff ladder before exiting (#1098).
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => return Ok(()),
                            _ = tokio::time::sleep(current_backoff) => {}
                        }
                        current_backoff = (current_backoff * 2).min(timings.max_backoff);
                    }
                }
            }
        })
    }
}

impl<Source: BlockchainSource> Drop for NodeBackedChainIndex<Source> {
    /// Cooperative cancellation on drop: signals the sync worker (and any
    /// other futures racing against `cancel_token.cancelled()`) to exit
    /// promptly when the indexer goes out of scope.
    ///
    /// Tests that drop the indexer without calling [`Self::shutdown`] —
    /// which is most of them — used to rely on the harness sleeping in its
    /// post-iter poll long enough that the worker was parked at its sync
    /// loop's interval-sleep before runtime teardown raced a mid-iter LMDB
    /// write. With body-level cancellation in the worker (`tokio::select!`
    /// on `cancel_token.cancelled()` wrapping the iter body), the worker
    /// exits at its next await checkpoint instead.
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

/// A clone-safe *read-only* view onto a running [`NodeBackedChainIndex`].
///
/// Designed for concurrent efficiency.
///
/// [`NodeBackedChainIndexSubscriber`] can safely be cloned and dropped freely.
#[derive(Clone, Debug)]
pub struct NodeBackedChainIndexSubscriber<Source: BlockchainSource = ValidatorConnector> {
    mempool: mempool::MempoolSubscriber,
    non_finalized_state: Arc<ArcSwapOption<crate::NonFinalizedState<Source>>>,
    finalized_state: finalised_state::reader::DbReader,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: Source,
}

async fn compact_block_from_source<Source: BlockchainSource>(
    source: &Source,
    network: ZebraNetwork,
    height: types::Height,
    pool_types: &PoolTypeFilter,
) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, ChainIndexError> {
    let Some(block) = source
        .get_block(HashOrHeight::Height(zebra_chain::block::Height(height.0)))
        .await
        .map_err(ChainIndexError::backing_validator)?
    else {
        return Ok(None);
    };

    let block_height = block
        .coinbase_height()
        .map(|height| types::Height(height.0))
        .ok_or_else(|| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "validator returned a block without a height",
            ))
        })?;
    if block_height != height {
        return Err(ChainIndexError::backing_validator(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "validator returned block at height {}, expected {}",
                block_height.0, height.0
            ),
        )));
    }

    let tree_roots = source
        .get_commitment_tree_roots(types::BlockHash::from(block.hash()))
        .await
        .map_err(ChainIndexError::backing_validator)?;
    let (sapling_root, sapling_size, orchard_root, orchard_size) =
        TreeRootData::new(tree_roots.0, tree_roots.1).extract_with_defaults();

    let metadata = BlockMetadata::new(
        sapling_root,
        sapling_size.try_into().map_err(|_| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sapling commitment tree size overflow",
            ))
        })?,
        orchard_root,
        orchard_size.try_into().map_err(|_| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "orchard commitment tree size overflow",
            ))
        })?,
        // TODO: Define an empty value https://github.com/zingolabs/zaino/issues/1158
        ChainWork::from_u256(0.into()),
        network,
    );
    let indexed_block =
        IndexedBlock::try_from(BlockWithMetadata::new(&block, metadata)).map_err(|error| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            ))
        })?;

    Ok(Some(compact_block_with_pool_types(
        indexed_block.to_compact_block(),
        &pool_types.to_pool_types_vector(),
    )))
}

impl<Source: BlockchainSource> NodeBackedChainIndexSubscriber<Source> {
    fn source(&self) -> &Source {
        &self.source
    }

    /// Returns the combined status of all chain index components.
    pub fn combined_status(&self) -> StatusType {
        let finalized_status = self.finalized_state.status();
        let mempool_status = self.mempool.status();
        let combined_status = self
            .status
            .load()
            .combine(finalized_status)
            .combine(mempool_status);
        self.status.store(combined_status);
        combined_status
    }

    /// Returns the number of transparent outputs of `txid` that are currently unspent in the
    /// finalised state. Returns 0 if `txid` is not indexed by the finalised state.
    ///
    /// Used by `get_tx_out_set_info` to seed the per-transaction unspent counter for prev
    /// transactions first encountered as a non-finalised-state spend.
    async fn count_finalised_unspent_outputs(
        &self,
        txid: TransactionHash,
    ) -> Result<u64, ChainIndexError> {
        let Some(tx_location) = self
            .finalized_state
            .get_tx_location(&txid)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?
        else {
            return Ok(0);
        };

        let Some(transparent) = self
            .finalized_state
            .get_transparent(tx_location)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?
        else {
            return Ok(0);
        };

        // Skip unspendable outputs (matches `is_unspendable_tx_out` semantics used by the
        // accumulator). NonStandard outputs are never in the UTXO set, so they must not count
        // toward a transaction's "remaining unspent" tally.
        use crate::chain_index::types::db::metadata::is_unspendable_tx_out;
        let outpoints: Vec<Outpoint> = transparent
            .outputs()
            .iter()
            .enumerate()
            .filter(|(_, out)| !is_unspendable_tx_out(out))
            .map(|(i, _)| Outpoint::new(txid.0, i as u32))
            .collect();

        if outpoints.is_empty() {
            return Ok(0);
        }

        let spenders = self
            .finalized_state
            .get_outpoint_spenders(outpoints)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?;

        Ok(spenders.into_iter().filter(|s| s.is_none()).count() as u64)
    }

    async fn get_fullblock_bytes_from_node(
        &self,
        id: HashOrHeight,
    ) -> Result<Option<Vec<u8>>, ChainIndexError> {
        self.source()
            .get_block(id)
            .await
            .map_err(ChainIndexError::backing_validator)?
            .map(|bk| {
                bk.zcash_serialize_to_vec()
                    .map_err(ChainIndexError::backing_validator)
            })
            .transpose()
    }

    async fn get_compact_block_from_node(
        &self,
        height: types::Height,
        pool_types: &PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, ChainIndexError> {
        compact_block_from_source(self.source(), self.network.clone(), height, pool_types).await
    }

    async fn get_indexed_block_height(
        &self,
        snapshot: &NonfinalizedBlockCacheSnapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, ChainIndexError> {
        // ChainIndex step 2:
        match snapshot.blocks.get(&hash).cloned() {
            Some(block) => Ok(snapshot
                // ChainIndex step 3:
                .heights_to_hashes
                .values()
                .find(|h| **h == hash)
                // Canonical height is None for blocks not on the best chain
                .map(|_| block.context.index.height)),
            None => self
                // ChainIndex step 4:
                .finalized_state
                .get_block_height(hash)
                .await
                .map_err(|e| ChainIndexError::database_hole(hash, Some(Box::new(e)))),
        }
    }

    /**
    Searches finalized and non-finalized chains for any blocks containing the transaction.
    Ordered with finalized blocks first.

    WARNING: there might be multiple chains, each containing a block with the transaction.
    */
    async fn blocks_containing_transaction<'snapshot, 'self_lt, 'iter>(
        &'self_lt self,
        snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> Result<impl Iterator<Item = IndexedBlock> + use<'iter, Source>, FinalisedStateError>
    where
        'snapshot: 'iter,
        'self_lt: 'iter,
    {
        let finalized_blocks_containing_transaction = match self
            .finalized_state
            .get_tx_location(&types::TransactionHash(txid))
            .await?
        {
            Some(tx_location) => {
                self.finalized_state
                    .get_chain_block_by_height(crate::Height(tx_location.block_height()))
                    .await?
            }

            None => None,
        }
        .into_iter();
        let non_finalized_blocks_containing_transaction =
            snapshot.blocks.values().filter_map(move |block| {
                block.transactions().iter().find_map(|transaction| {
                    if transaction.txid().0 == txid {
                        Some(block.clone())
                    } else {
                        None
                    }
                })
            });
        Ok(finalized_blocks_containing_transaction
            .chain(non_finalized_blocks_containing_transaction))
    }

    async fn get_block_height_passthrough(
        &self,
        max_serviceable_height: &types::Height,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, ChainIndexError> {
        //ChainIndex step 5:
        match self
            .source()
            .get_block(HashOrHeight::Hash(hash.into()))
            .await
        {
            Ok(Some(block)) => {
                // At this point, we know that
                // the block is in the VALIDATOR.
                match block.coinbase_height() {
                    None => {
                        // the block is in the VALIDATOR. but doesnt have a height. That would imply a bug.
                        Err(ChainIndexError::validator_data_error_block_coinbase_height_missing())
                    }
                    Some(height) => {
                        // The VALIDATOR returned a block with a height.
                        // However, there is as of yet no guaranteed the Block is FINALIZED
                        if height <= *max_serviceable_height {
                            Ok(Some(types::Height::from(height)))
                        } else {
                            // non-finalized block
                            // no passthrough
                            Ok(None)
                        }
                    }
                }
            }
            Ok(None) => {
                // the block is neither in the INDEXER nor VALIDATOR
                Ok(None)
            }
            Err(e) => Err(ChainIndexError::backing_validator(e)),
        }
    }

    // Get the height of the mempool
    fn get_mempool_height(&self, snapshot: &ChainIndexSnapshot) -> Option<types::Height> {
        let ChainIndexSnapshot::NonFinalizedStateExists {
            non_finalized_snapshot,
        } = snapshot
        else {
            return None;
        };

        non_finalized_snapshot
            .blocks
            .iter()
            .find(|(hash, _block)| **hash == self.mempool.mempool_chain_tip())
            .map(|(_hash, block)| block.height())
    }

    fn mempool_branch_id(&self, snapshot: &ChainIndexSnapshot) -> Option<u32> {
        self.get_mempool_height(snapshot).and_then(|height| {
            ConsensusBranchId::current(&self.network, zebra_chain::block::Height::from(height + 1))
                .map(u32::from)
        })
    }
}

impl<Source: BlockchainSource> Status for NodeBackedChainIndexSubscriber<Source> {
    fn status(&self) -> StatusType {
        self.combined_status()
    }
}

impl<Source: BlockchainSource> ChainIndex for NodeBackedChainIndexSubscriber<Source> {
    type Snapshot = ChainIndexSnapshot;
    type Error = ChainIndexError;

    // ********** Utility methods **********

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    async fn snapshot_nonfinalized_state(&self) -> Result<Self::Snapshot, Self::Error> {
        match self.non_finalized_state.load().as_ref() {
            Some(non_finalised_state) => Ok(ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot: non_finalised_state.get_snapshot(),
            }),
            None => {
                let height = self
                    .source
                    .get_best_block_height()
                    .await
                    .map_err(ChainIndexError::backing_validator)?
                    .ok_or(ChainIndexError::database_hole(
                        "validator has no best block",
                        None,
                    ))?;
                let validator_finalized_height = finalized_height_floor(height.0);
                Ok(ChainIndexSnapshot::StillSyncingFinalizedState {
                    validator_finalized_height,
                })
            }
        }
    }

    // ********** Block methods **********

    /// Returns Some(Height) for the given block hash *if* it is currently in the best chain.
    ///
    /// Returns None if the specified block is not in the best chain or is not found.
    ///
    /// Used for hash based block lookup (random access).
    async fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, Self::Error> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height
        // todo: possible efficiency boost by checking mempool for a negative?

        // ChainIndex steps 2-4:
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                self.get_indexed_block_height(non_finalized_snapshot, hash)
                    .await
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                self.get_block_height_passthrough(validator_finalized_height, hash)
                    .await
            } // ChainIndex step 5
        }
    }

    /// Returns Some(BlockHash) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    async fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
    ) -> Result<Option<types::BlockHash>, Self::Error> {
        // First check non-finalised state.
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => match non_finalized_snapshot
                .heights_to_hashes
                .get(&height)
                .copied()
            {
                Some(block_hash) => Ok(Some(block_hash)),
                // If not found check finalised state.
                None => self
                    .finalized_state
                    .get_block_hash(height)
                    .await
                    .map_err(Into::into),
            },

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                if height <= *validator_finalized_height {
                    // If still syncing try to fetch from backing validator (*passthrough*).
                    //
                    // Note this requires fetching the full block from the backing node.
                    match self
                        .source()
                        .get_block(HashOrHeight::Height(height.into()))
                        .await
                        .map_err(ChainIndexError::backing_validator)?
                    {
                        Some(block) => Ok(Some(block.hash().into())),
                        None => Ok(None),
                    }
                } else {
                    // The requested block is non-finalized
                    // We can't safely serve it via passthrough
                    Ok(None)
                }
            }
        }
    }

    /// Returns Some(IndexedBlock) for the given block hash.
    ///
    /// Returns None if the specified block is not found.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    async fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        match snapshot.get_chainblock_by_hash(target_hash) {
            Some(block) => Ok(Some(block.clone())),
            None => match self.get_block_height(snapshot, *target_hash).await {
                Ok(Some(height)) => Ok(self
                    .finalized_state
                    .get_chain_block_by_height(height)
                    .await?),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            },
        }
    }

    /// Returns Some(IndexedBlock) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    async fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        match snapshot.get_chainblock_by_height(target_height) {
            Some(block) => Ok(Some(block.clone())),
            None => Ok(self
                .finalized_state
                .get_chain_block_by_height(*target_height)
                .await?),
        }
    }

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified start height
    /// is greater than the snapshot's tip and greater
    /// than the validator's finalized height (100 blocks below tip)
    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: std::option::Option<types::Height>,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height

        // The lower of the end of the provided range, and the highest block we can serve
        let end = end
            .unwrap_or(*snapshot.max_serviceable_height())
            .min(*snapshot.max_serviceable_height());
        // Serve as high as we can, or to the provided end if it's lower
        if start <= *snapshot.max_serviceable_height().min(&end) {
            Some(
                futures::stream::iter((start.0)..=(end.0)).then(move |height| async move {
                    // For blocks above validator_finalized_height, it's not reorg-safe to get blocks by height. It is reorg-safe to get blocks by hash. What we need to do in this case is use our snapshot index to look up the hash at a given height, and then get that hash from the validator.
                    // This is why we now look in the index.
                    match self
                        .finalized_state
                        .get_block_hash(types::Height(height))
                        .await
                    {
                        Ok(Some(hash)) => {
                            return self
                                .get_fullblock_bytes_from_node(HashOrHeight::Hash(hash.into()))
                                .await?
                                .ok_or(ChainIndexError::database_hole(hash, None))
                        }
                        Err(e) => Err(ChainIndexError {
                            kind: ChainIndexErrorKind::InternalServerError,
                            message: "".to_string(),
                            source: Some(Box::new(e)),
                        }),
                        Ok(None) => {
                            match snapshot.get_chainblock_by_height(&types::Height(height)) {
                                Some(block) => {
                                    return self
                                        .get_fullblock_bytes_from_node(HashOrHeight::Hash(
                                            (*block.hash()).into(),
                                        ))
                                        .await?
                                        .ok_or(ChainIndexError::database_hole(block.hash(), None))
                                }
                                None => self
                                    // usually getting by height is not reorg-safe, but here, height is known to be below or equal to validator_finalized_height.
                                    .get_fullblock_bytes_from_node(HashOrHeight::Height(
                                        zebra_chain::block::Height(height),
                                    ))
                                    .await?
                                    .ok_or(ChainIndexError::database_hole(height, None)),
                            }
                        }
                    }
                }),
            )
        } else {
            None
        }
    }

    /// Returns the *compact* block for the given height.
    ///
    /// Returns `None` if the specified `height` is greater than the snapshot's tip.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    ///
    /// Returns None if the specified height
    /// is greater than the snapshot's tip
    ///
    /// **NOTE: This Method is currently not "passthrough aware", this should be added by
    /// fetching block data from the backing validator when not locally available.**
    async fn get_compact_block(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error> {
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                if height <= non_finalized_snapshot.best_tip.height {
                    Ok(Some(match snapshot.get_chainblock_by_height(&height) {
                        Some(block) => compact_block_with_pool_types(
                            block.to_compact_block(),
                            &pool_types.to_pool_types_vector(),
                        ),
                        None => {
                            match self
                                .finalized_state
                                .get_compact_block(height, pool_types.clone())
                                .await
                            {
                                Ok(block) => block,
                                Err(_) => self
                                    .get_compact_block_from_node(height, &pool_types)
                                    .await?
                                    .ok_or(ChainIndexError::database_hole(height, None))?,
                            }
                        }
                    }))
                } else {
                    Ok(None)
                }
            }

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height: _,
                //TODO: Once we make chainwork an option field we should be able to
                // support passthrougth for this
            } => Ok(None),
        }
    }

    /// Streams *compact* blocks for an inclusive height range.
    ///
    /// Returns `Ok(None)` if the request is descending and `start_height` exceeds the chain tip.
    /// For ascending requests that exceed the tip, returns a stream that ends with an
    /// `out_of_range` error after all available blocks have been sent.
    ///
    /// - The stream covers `[start_height, end_height]` (inclusive).
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    ///
    /// **NOTE: This Method is currently not "passthrough aware", this should be added by
    /// fetching block data from the backing validator when not locally available.**
    #[allow(clippy::type_complexity)]
    async fn get_compact_block_stream(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, Self::Error> {
        let chain_tip_height = self.best_chaintip(nonfinalized_snapshot).await?.height;

        // The nonfinalized cache holds the tip block plus the previous 99 blocks (100 total),
        // so the lowest possible cached height is `tip - 99` (saturating at 0).
        let lowest_nonfinalized_height = types::Height(chain_tip_height.0.saturating_sub(99));

        let is_ascending = start_height <= end_height;

        // Descending: the first block we'd try to return is already above the tip → error immediately.
        if !is_ascending && start_height > chain_tip_height {
            return Ok(None);
        }

        let pool_types_vector = pool_types.to_pool_types_vector();

        // For ascending requests that extend past the tip: cap the streaming range at the tip,
        // then append a trailing out_of_range error after all valid blocks have been sent.
        let needs_out_of_range = is_ascending && end_height > chain_tip_height;
        let capped_end_height = if needs_out_of_range {
            chain_tip_height
        } else {
            end_height
        };

        // Pre-create any finalized-state stream(s) we will need so that errors are returned
        // from this method (not deferred into the spawned task).
        let finalized_stream: Option<CompactBlockStream> = if is_ascending {
            if start_height < lowest_nonfinalized_height {
                let finalized_end_height = types::Height(std::cmp::min(
                    capped_end_height.0,
                    lowest_nonfinalized_height.0.saturating_sub(1),
                ));

                if start_height <= finalized_end_height {
                    Some(
                        self.finalized_state
                            .get_compact_block_stream(
                                start_height,
                                finalized_end_height,
                                pool_types.clone(),
                            )
                            .await
                            .map_err(ChainIndexError::from)?,
                    )
                } else {
                    None
                }
            } else {
                None
            }
        // Serve in reverse order.
        } else if end_height < lowest_nonfinalized_height {
            let finalized_start_height = if start_height < lowest_nonfinalized_height {
                start_height
            } else {
                types::Height(lowest_nonfinalized_height.0.saturating_sub(1))
            };

            Some(
                self.finalized_state
                    .get_compact_block_stream(
                        finalized_start_height,
                        end_height,
                        pool_types.clone(),
                    )
                    .await
                    .map_err(ChainIndexError::from)?,
            )
        } else {
            None
        };

        let nonfinalized_snapshot = nonfinalized_snapshot.clone();
        let source = self.source.clone();
        let network = self.network.clone();
        let pool_types_for_node = pool_types.clone();
        // TODO: Investigate whether channel size should be changed, added to config, or set dynamically based on resources.
        let (channel_sender, channel_receiver) = tokio::sync::mpsc::channel(128);

        tokio::spawn(async move {
            if is_ascending {
                // 1) Finalized segment (if any), ascending.
                if let Some(mut finalized_stream) = finalized_stream {
                    while let Some(stream_item) = finalized_stream.next().await {
                        if channel_sender.send(stream_item).await.is_err() {
                            return;
                        }
                    }
                }

                // 2) Nonfinalized segment, ascending.
                let nonfinalized_start_height =
                    types::Height(std::cmp::max(start_height.0, lowest_nonfinalized_height.0));

                for height_value in nonfinalized_start_height.0..=capped_end_height.0 {
                    let Some(indexed_block) = nonfinalized_snapshot
                        .get_chainblock_by_height(&types::Height(height_value))
                    else {
                        match compact_block_from_source(
                            &source,
                            network.clone(),
                            types::Height(height_value),
                            &pool_types_for_node,
                        )
                        .await
                        {
                            Ok(Some(compact_block)) => {
                                if channel_sender.send(Ok(compact_block)).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                            Ok(None) => {
                                let _ = channel_sender
                                    .send(Err(tonic::Status::internal(format!(
                                        "Internal error, missing nonfinalized block at height [{height_value}].",
                                    ))))
                                    .await;
                                return;
                            }
                            Err(error) => {
                                let _ = channel_sender
                                    .send(Err(tonic::Status::internal(error.to_string())))
                                    .await;
                                return;
                            }
                        }
                    };
                    let compact_block = compact_block_with_pool_types(
                        indexed_block.to_compact_block(),
                        &pool_types_vector,
                    );
                    if channel_sender.send(Ok(compact_block)).await.is_err() {
                        return;
                    }
                }
                // If the original end_height was above the tip, signal out_of_range after all valid blocks.
                if needs_out_of_range {
                    let _ = channel_sender
                          .send(Err(tonic::Status::out_of_range(format!(
                              "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                              end_height.0, chain_tip_height.0,
                          ))))
                          .await;
                }
            } else {
                // 1) Nonfinalized segment, descending.
                if start_height >= lowest_nonfinalized_height {
                    let nonfinalized_end_height =
                        types::Height(std::cmp::max(end_height.0, lowest_nonfinalized_height.0));

                    for height_value in (nonfinalized_end_height.0..=start_height.0).rev() {
                        let Some(indexed_block) = nonfinalized_snapshot
                            .get_chainblock_by_height(&types::Height(height_value))
                        else {
                            match compact_block_from_source(
                                &source,
                                network.clone(),
                                types::Height(height_value),
                                &pool_types_for_node,
                            )
                            .await
                            {
                                Ok(Some(compact_block)) => {
                                    if channel_sender.send(Ok(compact_block)).await.is_err() {
                                        return;
                                    }
                                    continue;
                                }
                                Ok(None) => {
                                    let _ = channel_sender
                                        .send(Err(tonic::Status::internal(format!(
                                            "Internal error, missing nonfinalized block at height [{height_value}].",
                                        ))))
                                        .await;
                                    return;
                                }
                                Err(error) => {
                                    let _ = channel_sender
                                        .send(Err(tonic::Status::internal(error.to_string())))
                                        .await;
                                    return;
                                }
                            }
                        };
                        let compact_block = compact_block_with_pool_types(
                            indexed_block.to_compact_block(),
                            &pool_types_vector,
                        );
                        if channel_sender.send(Ok(compact_block)).await.is_err() {
                            return;
                        }
                    }
                }

                // 2) Finalized segment (if any), descending.
                if let Some(mut finalized_stream) = finalized_stream {
                    while let Some(stream_item) = finalized_stream.next().await {
                        if channel_sender.send(stream_item).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Ok(Some(CompactBlockStream::new(channel_receiver)))
    }

    // ********** Transaction methods **********

    /// given a transaction id, returns the transaction
    /// and the consensus branch ID for the block the transaction
    /// is in
    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, Self::Error> {
        // ChainIndex step 1
        if let Some(mempool_tx) = self
            .mempool
            .get_transaction(&mempool::MempoolKey {
                txid: txid.to_rpc_hex(),
            })
            .await
        {
            let bytes = mempool_tx.serialized_tx.as_ref().as_ref().to_vec();
            let mempool_branch_id = self.mempool_branch_id(snapshot);

            return Ok(Some((bytes, mempool_branch_id)));
        }

        let Some((transaction, location)) = self
            .source()
            .get_transaction(*txid)
            .await
            .map_err(ChainIndexError::backing_validator)?
        else {
            return Ok(None);
        };
        // as the reorg process cannot modify a transaction
        // it's safe to serve nonfinalized state directly here
        let height = match location {
            GetTransactionLocation::BestChain(height) => height,
            GetTransactionLocation::NonbestChain => {
                // if the tranasction isn't on the best chain
                // check our indexes. We need to find out the height from our index
                // to determine the consensus branch ID
                let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
                    // If we don't have a block containing the transaction
                    // locally and the transaction's not on the validator's
                    // best chain, we can't determine its consensus branch ID
                    return Ok(None);
                };

                match self
                    .blocks_containing_transaction(non_finalized_snapshot, txid.0)
                    .await?
                    .next()
                {
                    Some(block) => block.context.index.height.into(),
                    // As above Ok(None)
                    None => return Ok(None),
                }
            }
            // We've already checked the mempool. Should be unreachable?
            // todo: error here?
            GetTransactionLocation::Mempool => return Ok(None),
        };

        Ok(Some((
            zebra_chain::transaction::SerializedTransaction::from(transaction)
                .as_ref()
                .to_vec(),
            ConsensusBranchId::current(&self.network, height).map(u32::from),
        )))
    }

    /// Given a transaction ID, returns all known blocks containing this transaction
    ///
    /// If the transaction is in the mempool, it will be in the `BestChainLocation`
    /// if the mempool and snapshot are up-to-date, and the `NonBestChainLocation` set
    /// if the snapshot is out-of-date compared to the mempool
    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), ChainIndexError> {
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                let blocks_containing_transaction = self
                    .blocks_containing_transaction(non_finalized_snapshot, txid.0)
                    .await?
                    .collect::<Vec<_>>();
                let Some(start_of_nonfinalized) =
                    non_finalized_snapshot.heights_to_hashes.keys().min()
                else {
                    return Err(ChainIndexError::database_hole("no blocks", None));
                };
                let mut best_chain_block = blocks_containing_transaction
                    .iter()
                    .find(|block| {
                        non_finalized_snapshot
                            .heights_to_hashes
                            .get(&block.height())
                            == Some(block.hash())
                            || block.height() < *start_of_nonfinalized
                        // this block is either in the best chain ``heights_to_hashes`` or finalized.
                    })
                    .map(|block| BestChainLocation::Block(*block.hash(), block.height()));
                let mut non_best_chain_blocks: HashSet<NonBestChainLocation> =
                    blocks_containing_transaction
                        .iter()
                        .filter(|block| {
                            non_finalized_snapshot
                                .heights_to_hashes
                                .get(&block.height())
                                != Some(block.hash())
                                && block.height() >= *start_of_nonfinalized
                        })
                        .map(|block| NonBestChainLocation::Block(*block.hash(), block.height()))
                        .collect();
                let in_mempool = self
                    .mempool
                    .contains_txid(&mempool::MempoolKey {
                        txid: txid.to_rpc_hex(),
                    })
                    .await;
                if in_mempool {
                    let mempool_tip_hash = self.mempool.mempool_chain_tip();
                    if mempool_tip_hash == non_finalized_snapshot.best_tip.hash {
                        if best_chain_block.is_some() {
                            return Err(ChainIndexError {
                        kind: ChainIndexErrorKind::InvalidSnapshot,
                        message:
                            "Best chain and up-to-date mempool both contain the same transaction"
                                .to_string(),
                        source: None,
                    });
                        } else {
                            best_chain_block = Some(BestChainLocation::Mempool(
                                non_finalized_snapshot.best_tip.height + 1,
                            ));
                        }
                    } else {
                        // the best chain and the mempool have divergent tip hashes
                        // get a new snapshot and use it to find the height of the mempool
                        if let ChainIndexSnapshot::NonFinalizedStateExists {
                            non_finalized_snapshot: new_snapshot,
                        } = self.snapshot_nonfinalized_state().await?
                        {
                            let target_height =
                                new_snapshot.blocks.iter().find_map(|(hash, block)| {
                                    if *hash == mempool_tip_hash {
                                        Some(block.height() + 1)
                                        // found the block that is the tip that the mempool is hanging on to
                                    } else {
                                        None
                                    }
                                });
                            non_best_chain_blocks
                                .insert(NonBestChainLocation::Mempool(target_height));
                        }
                    }
                }
                Ok((best_chain_block, non_best_chain_blocks))
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                if let Some((_transaction, GetTransactionLocation::BestChain(height))) = self
                    .source()
                    .get_transaction(*txid)
                    .await
                    .map_err(ChainIndexError::backing_validator)?
                {
                    if height <= *validator_finalized_height {
                        if let Some(block) = self
                            .source()
                            .get_block(HashOrHeight::Height(height))
                            .await
                            .map_err(ChainIndexError::backing_validator)?
                        {
                            return Ok((
                                Some(BestChainLocation::Block(block.hash().into(), height.into())),
                                HashSet::new(),
                            ));
                        }
                    }
                }
                Ok((None, HashSet::new()))
            }
        }
    }

    /// Returns all txids currently in the mempool.
    async fn get_mempool_txids(&self) -> Result<Vec<types::TransactionHash>, Self::Error> {
        self.mempool
            .get_mempool()
            .await
            .into_iter()
            .map(|(txid_key, _)| {
                TransactionHash::from_hex(&txid_key.txid)
                    .map_err(ChainIndexError::backing_validator)
            })
            .collect::<Result<_, _>>()
    }

    /// Returns all transactions currently in the mempool, filtered by `exclude_list`.
    ///
    /// The `exclude_list` may contain shortened transaction ID hex prefixes (client-endian).
    /// The transaction IDs in the Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    async fn get_mempool_transactions(
        &self,
        exclude_list: Vec<String>,
    ) -> Result<Vec<Vec<u8>>, Self::Error> {
        // Use the mempool's own filtering (it already handles client-endian shortened prefixes).
        let pairs: Vec<(mempool::MempoolKey, mempool::MempoolValue)> =
            self.mempool.get_filtered_mempool(exclude_list).await;

        // Transform to the Vec<Vec<u8>> that the trait requires.
        let bytes: Vec<Vec<u8>> = pairs
            .into_iter()
            .map(|(_, v)| v.serialized_tx.as_ref().as_ref().to_vec())
            .collect();

        Ok(bytes)
    }

    /// Returns a stream of mempool transactions, ending the stream when the chain tip block hash
    /// changes (a new block is mined or a reorg occurs).
    ///
    /// If a snapshot is given and the chain tip has changed from the given spanshot, returns None.
    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let non_finalized_snapshot = match snapshot {
            Some(s) => match s {
                ChainIndexSnapshot::NonFinalizedStateExists {
                    non_finalized_snapshot,
                } => Some(non_finalized_snapshot),
                // If we're still syncing the finalized state, the chain tip
                // is newer than the snapshot's tip. Return None.
                ChainIndexSnapshot::StillSyncingFinalizedState { .. } => return None,
            },
            None => None,
        };
        let expected_chain_tip = non_finalized_snapshot.map(|snapshot| snapshot.best_tip.hash);
        let mut subscriber = self.mempool.clone();

        match subscriber
            .get_mempool_stream(expected_chain_tip)
            .now_or_never()
        {
            Some(Ok((in_rx, _handle))) => {
                let (out_tx, out_rx) =
                    tokio::sync::mpsc::channel::<Result<Vec<u8>, ChainIndexError>>(32);

                tokio::spawn(async move {
                    let mut in_stream = tokio_stream::wrappers::ReceiverStream::new(in_rx);
                    while let Some(item) = in_stream.next().await {
                        match item {
                            Ok((_key, value)) => {
                                let _ = out_tx
                                    .send(Ok(value.serialized_tx.as_ref().as_ref().to_vec()))
                                    .await;
                            }
                            Err(e) => {
                                let _ = out_tx
                                    .send(Err(ChainIndexError::child_process_status_error(
                                        "mempool", e,
                                    )))
                                    .await;
                                break;
                            }
                        }
                    }
                });

                Some(tokio_stream::wrappers::ReceiverStream::new(out_rx))
            }
            Some(Err(crate::error::MempoolError::IncorrectChainTip { .. })) => None,
            Some(Err(e)) => {
                let (out_tx, out_rx) =
                    tokio::sync::mpsc::channel::<Result<Vec<u8>, ChainIndexError>>(1);
                let _ = out_tx.try_send(Err(e.into()));
                Some(tokio_stream::wrappers::ReceiverStream::new(out_rx))
            }
            None => {
                // Should not happen because the inner tip check is synchronous, but fail safe.
                let (out_tx, out_rx) =
                    tokio::sync::mpsc::channel::<Result<Vec<u8>, ChainIndexError>>(1);
                let _ = out_tx.try_send(Err(ChainIndexError::child_process_status_error(
                    "mempool",
                    crate::error::StatusError {
                        server_status: crate::StatusType::RecoverableError,
                    },
                )));
                Some(tokio_stream::wrappers::ReceiverStream::new(out_rx))
            }
        }
    }

    // ********** Chain methods **********

    /// For a given block,
    /// find its newest main-chain ancestor,
    /// or the block itself if it is on the main-chain.
    /// Returns Ok(None) if no fork point found. This is not an error,
    /// as zaino does not guarentee knowledge of all sidechain data.
    async fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height, guaranteed to return None
        // todo: possible efficiency boost by checking mempool for a negative?

        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                match non_finalized_snapshot.get_chainblock_by_hash(hash) {
                    Some(block) => {
                        // At this point, we know that
                        // The block is non-FINALIZED in the INDEXER
                        // ChainIndex step 3:
                        if non_finalized_snapshot
                            .heights_to_hashes
                            .get(&block.height())
                            == Some(block.hash())
                        {
                            // The block is in the best chain.
                            Ok(Some((*block.hash(), block.height())))
                        } else {
                            // Otherwise, it's non-best chain! Grab its parent, and recurse
                            Box::pin(self.find_fork_point(snapshot, &block.context.parent_hash))
                                .await
                            // gotta pin recursive async functions to prevent infinite-sized
                            // Future-implementing types
                        }
                    }
                    None => {
                        // At this point, we know that
                        // the block is NOT non-FINALIZED in the INDEXER.
                        // as the non finalzed state is known to be populated,
                        // we now check the finalized state
                        match self.finalized_state.get_block_height(*hash).await {
                            Ok(Some(height)) => {
                                // the block is FINALIZED in the INDEXER
                                Ok(Some((*hash, height)))
                            }
                            Err(e) => Err(ChainIndexError::database_hole(hash, Some(Box::new(e)))),
                            Ok(None) => Ok(None),
                        }
                    }
                }
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                // We're not fully synced, so we pass through.
                // Now, we ask the VALIDATOR.
                // ChainIndex step 5
                match self
                    .source()
                    .get_block(HashOrHeight::Hash(zebra_chain::block::Hash::from(*hash)))
                    .await
                {
                    Ok(Some(block)) => {
                        // At this point, we know that
                        // the block is in the VALIDATOR.
                        match block.coinbase_height() {
                            None => {
                                // the block is in the VALIDATOR. but doesnt have a height. That would imply a bug.
                                Err(ChainIndexError::validator_data_error_block_coinbase_height_missing())
                            }
                            Some(height) => {
                                // The VALIDATOR returned a block with a height.
                                // However, there is as of yet no guaranteed the Block is FINALIZED
                                if height <= *validator_finalized_height {
                                    Ok(Some((
                                        types::BlockHash::from(block.hash()),
                                        types::Height::from(height),
                                    )))
                                } else {
                                    // non-finalized block
                                    // no passthrough
                                    Ok(None)
                                }
                            }
                        }
                    }

                    Ok(None) => {
                        // At this point, we know that
                        // the block is NOT FINALIZED in the VALIDATOR.
                        // Return Ok(None) = no block found.
                        Ok(None)
                    }
                    Err(e) => Err(ChainIndexError::backing_validator(e)),
                }
            }
        }
    }

    /// Returns the block commitment tree data by hash
    async fn get_treestate(
        &self,
        // currently not implemented internally, fetches data from validator.
        // as this looks up the block by hash, and cares not if the
        // block is on the main chain or not, this is safe to pass through
        // even if the target block is non-finalized
        hash: &types::BlockHash,
    ) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>), Self::Error> {
        match self.source().get_treestate(*hash).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(ChainIndexError {
                kind: ChainIndexErrorKind::InternalServerError,
                message: "failed to fetch treestate from validator".to_string(),
                source: Some(Box::new(e)),
            }),
        }
    }

    /// Gets the subtree roots of a given pool and the end heights of each root,
    /// starting at the provided index, up to an optional maximum number of roots.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error> {
        self.source()
            .get_subtree_roots(pool, start_index, max_entries)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // ********** Transparent address history methods **********

    /// Returns all changes for the given transparent addresses.
    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> Result<GetAddressDeltasResponse, Self::Error> {
        self.source()
            .get_address_deltas(params)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns the total transparent balance for the given addresses.
    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<AddressBalance, Self::Error> {
        self.source()
            .get_address_balance(address_strings)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns the transaction ids made by the given transparent addresses.
    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<types::TransactionHash>, Self::Error> {
        self.source()
            .get_address_txids(request)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns all unspent transparent outputs for the given addresses.
    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error> {
        self.source()
            .get_address_utxos(address_strings)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // ********** Metadata methods **********

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    async fn get_mempool_info(&self) -> MempoolInfo {
        self.mempool.get_mempool_info().await
    }

    async fn best_chaintip(&self, snapshot: &Self::Snapshot) -> Result<BlockIndex, Self::Error> {
        Ok(match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.best_tip,

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                BlockIndex {
                    height: *validator_finalized_height,
                    hash: self
                        .source()
                        // TODO: do something more efficient than getting the whole block
                        .get_block(HashOrHeight::Height((*validator_finalized_height).into()))
                        .await
                        .map_err(|e| {
                            ChainIndexError::database_hole(
                                validator_finalized_height,
                                Some(Box::new(e)),
                            )
                        })?
                        .ok_or(ChainIndexError::database_hole(
                            validator_finalized_height,
                            None,
                        ))?
                        .hash()
                        .into(),
                }
            }
        })
    }

    async fn get_tx_out_set_info(&self) -> Result<GetTxOutSetInfoResponse, Self::Error> {
        use crate::chain_index::types::db::metadata::{
            is_unspendable_tx_out, ZAINO_TXOUTSET_ENTRY_LEN,
        };
        use hex::ToHex as _;
        use std::collections::HashMap;

        let snapshot = self.snapshot_nonfinalized_state().await?;
        let best_tip = self.best_chaintip(&snapshot).await?;

        let non_finalized_snapshot = match &snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot,
            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => {
                // Accumulator invariants are not established until the finalised state catches
                // up. Match zcashd's "stats collection failed" empty-object shape.
                return Ok(GetTxOutSetInfoResponse::Empty(EmptyTxOutSetInfo {}));
            }
        };

        let mut accumulator = self
            .finalized_state
            .get_tx_out_set_info_accumulator()
            .await
            .map_err(|e| {
                ChainIndexError::internal(format!(
                    "get_tx_out_set_info: finalised accumulator unavailable: {e}"
                ))
            })?;

        // Outputs created inside the non-finalised state, keyed by outpoint. Lets same-NFS
        // spends resolve their prev output without touching the finalised database.
        let mut nfs_created: HashMap<Outpoint, TxOutCompact> = HashMap::new();

        // Per-transaction "currently-unspent transparent outputs" counter across the combined
        // finalised + non-finalised UTXO set. Seeded lazily:
        // - For NFS-created txs: starts at 0 and increments on each output added.
        // - For purely-finalised prev txs first encountered as a spend: seeded by counting how
        //   many of that tx's transparent outputs are unspent in the finalised state right now.
        //
        // We only modify `accumulator.transactions` on 0↔>0 transitions of this counter; the
        // finalised accumulator already reflects the steady-state count for every tx not
        // touched by the NFS walk.
        let mut tx_unspent_count: HashMap<TransactionHash, u64> = HashMap::new();

        let mut heights: Vec<types::Height> = non_finalized_snapshot
            .heights_to_hashes
            .keys()
            .copied()
            .collect();
        heights.sort();

        for height in heights {
            let Some(block) = non_finalized_snapshot.get_chainblock_by_height(&height) else {
                return Err(ChainIndexError::internal(format!(
                    "get_tx_out_set_info: non-finalised snapshot height {height:?} has no block"
                )));
            };

            for tx in block.transactions() {
                let txid = *tx.txid();
                let transparent = tx.transparent();

                // Created outputs enter the UTXO set.
                //
                // NonStandard (unspendable) outputs are skipped at every level — the accumulator
                // never saw them on the finalised side either, so they must not contribute to
                // `transactions` or to the resolution map for later same-NFS spends.
                for (output_index, output) in transparent.outputs().iter().enumerate() {
                    if is_unspendable_tx_out(output) {
                        continue;
                    }
                    let outpoint = Outpoint::new(txid.0, output_index as u32);
                    accumulator
                        .apply_added_output(&outpoint, output)
                        .map_err(|e| ChainIndexError::internal(e.to_string()))?;
                    nfs_created.insert(outpoint, *output);

                    let entry = tx_unspent_count.entry(txid).or_insert(0);
                    let prev = *entry;
                    *entry += 1;
                    if prev == 0 {
                        // 0 -> >0 transition: this tx enters the in-set transaction count.
                        accumulator.transactions =
                            accumulator.transactions.checked_add(1).ok_or_else(|| {
                                ChainIndexError::internal(
                                    "get_tx_out_set_info: transactions counter overflow"
                                        .to_string(),
                                )
                            })?;
                    }
                }

                // Spent prev outputs leave the UTXO set.
                for input in transparent.inputs() {
                    if input.is_null_prevout() {
                        continue;
                    }

                    let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                    let prev_txid = TransactionHash::from(*outpoint.prev_txid());

                    let prev_out_from_nfs = nfs_created.remove(&outpoint);
                    let prev_out = match prev_out_from_nfs {
                        Some(out) => out,
                        None => self
                            .finalized_state
                            .get_previous_output(outpoint)
                            .await
                            .map_err(|e| {
                                ChainIndexError::internal(format!(
                                    "get_tx_out_set_info: finalised prev output for {outpoint:?} not found: {e}"
                                ))
                            })?,
                    };

                    accumulator
                        .apply_removed_output(&outpoint, &prev_out)
                        .map_err(|e| ChainIndexError::internal(e.to_string()))?;

                    // Seed the prev_txid unspent counter if this is the first time we touch it.
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        tx_unspent_count.entry(prev_txid)
                    {
                        let seed = self
                            .count_finalised_unspent_outputs(prev_txid)
                            .await
                            .map_err(|e| {
                                ChainIndexError::internal(format!(
                                    "get_tx_out_set_info: cannot seed unspent counter for {prev_txid:?}: {e}"
                                ))
                            })?;
                        e.insert(seed);
                    }

                    let entry = tx_unspent_count.get_mut(&prev_txid).expect("seeded above");
                    if *entry == 0 {
                        return Err(ChainIndexError::internal(format!(
                            "get_tx_out_set_info: tx {prev_txid:?} unspent counter underflow"
                        )));
                    }
                    *entry -= 1;
                    if *entry == 0 {
                        accumulator.transactions =
                            accumulator.transactions.checked_sub(1).ok_or_else(|| {
                                ChainIndexError::internal(
                                    "get_tx_out_set_info: transactions counter underflow"
                                        .to_string(),
                                )
                            })?;
                    }
                }
            }
        }

        // Invariant: bytes_serialized == transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN.
        let expected_bytes = accumulator
            .transaction_outputs
            .checked_mul(ZAINO_TXOUTSET_ENTRY_LEN)
            .ok_or_else(|| {
                ChainIndexError::internal(
                    "get_tx_out_set_info: bytes_serialized invariant overflow".to_string(),
                )
            })?;
        if accumulator.bytes_serialized != expected_bytes {
            return Err(ChainIndexError::internal(format!(
                "get_tx_out_set_info: bytes_serialized invariant violated (got {}, expected {})",
                accumulator.bytes_serialized, expected_bytes
            )));
        }

        let total_amount = accumulator.total_zatoshis as f64 / 1e8;
        let hash_serialized: String = accumulator.hash_serialized.encode_hex();
        let best_block: String = best_tip.hash.encode_hex();

        Ok(GetTxOutSetInfoResponse::Info(GetTxOutSetInfo {
            height: best_tip.height.0.into(),
            best_block,
            transactions: accumulator.transactions,
            txouts: accumulator.transaction_outputs,
            bytes_serialized: accumulator.bytes_serialized,
            hash_serialized,
            total_amount,
        }))
    }
}

/// The available shielded pools
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling
    Sapling,
    /// Orchard
    Orchard,
}

impl ShieldedPool {
    /// Returns the string representative of the given pool.
    ///
    /// Used for display purposes and in converting the strongly types `PoolType`
    /// struct into the string that the Zcash RPCs require as input.
    pub fn pool_string(&self) -> String {
        match self {
            ShieldedPool::Sapling => "sapling".to_string(),
            ShieldedPool::Orchard => "orchard".to_string(),
        }
    }
}

impl<T> NonFinalizedSnapshot for Arc<T>
where
    T: NonFinalizedSnapshot,
{
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_hash(target_hash)
    }

    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_height(target_height)
    }

    fn max_serviceable_height(&self) -> &types::Height {
        self.as_ref().max_serviceable_height()
    }
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock>;
    /// The maximum height that this snapshot can serve data for.
    fn max_serviceable_height(&self) -> &types::Height;
}

impl NonFinalizedSnapshot for NonfinalizedBlockCacheSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.blocks.iter().find_map(|(hash, chainblock)| {
            if hash == target_hash {
                Some(chainblock)
            } else {
                None
            }
        })
    }
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.heights_to_hashes.iter().find_map(|(height, hash)| {
            if height == target_height {
                self.get_chainblock_by_hash(hash)
            } else {
                None
            }
        })
    }

    fn max_serviceable_height(&self) -> &types::Height {
        &self.best_tip.height
    }
}

impl NonFinalizedSnapshot for ChainIndexSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.get_chainblock_by_hash(target_hash),

            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => None,
        }
    }

    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.get_chainblock_by_height(target_height),

            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => None,
        }
    }

    fn max_serviceable_height(&self) -> &types::Height {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.max_serviceable_height(),

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => validator_finalized_height,
        }
    }
}
