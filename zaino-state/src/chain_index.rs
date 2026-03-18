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

use crate::chain_index::non_finalised_state::BestTip;
use crate::chain_index::source::GetTransactionLocation;
use crate::chain_index::types::db::metadata::MempoolInfo;
use crate::chain_index::types::{BestChainLocation, NonBestChainLocation};
use crate::error::{ChainIndexError, ChainIndexErrorKind, FinalisedStateError};
use crate::status::Status;
use crate::{AtomicStatus, CompactBlockStream, NodeConnectionError, StatusType, SyncError};
use crate::{IndexedBlock, TransactionHash};
use std::collections::HashSet;
use std::{sync::Arc, time::Duration};

use futures::{FutureExt, Stream};
use hex::FromHex as _;
use non_finalised_state::NonfinalizedBlockCacheSnapshot;
use source::{BlockchainSource, ValidatorConnector};
use tokio_stream::StreamExt;
use tracing::info;
use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};
use zebra_chain::parameters::ConsensusBranchId;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_chain::serialization::ZcashSerialize;
use zebra_state::HashOrHeight;

pub mod encoding;
/// All state at least 100 blocks old
pub mod finalised_state;
/// State in the mempool, not yet on-chain
pub mod mempool;
/// State less than 100 blocks old, stored separately as it may be reorged
pub mod non_finalised_state;
/// BlockchainSource
pub mod source;
/// Common types used by the rest of this module
pub mod types;

#[cfg(test)]
mod tests;

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

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot;

    /// Returns Some(Height) for the given block hash *if* it is currently in the best chain.
    ///
    /// Returns None if the specified block is not in the best chain or is not found.
    fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<types::Height>, Self::Error>>;

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified end height
    /// is greater than the snapshot's tip
    // TO-TEST
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

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    fn get_mempool_info(&self) -> impl std::future::Future<Output = MempoolInfo>;
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
    non_finalized_state: std::sync::Arc<crate::NonFinalizedState<Source>>,
    finalized_db: std::sync::Arc<finalised_state::ZainoDB>,
    sync_loop_handle: Option<tokio::task::JoinHandle<Result<(), SyncError>>>,
    status: AtomicStatus,
}

impl<Source: BlockchainSource> NodeBackedChainIndex<Source> {
    /// Creates a new chainindex from a connection to a validator
    /// Currently this is a ReadStateService or JsonRpSeeConnector
    pub async fn new(
        source: Source,
        config: crate::config::BlockCacheConfig,
    ) -> Result<Self, crate::InitError> {
        use futures::TryFutureExt as _;

        let finalized_db =
            Arc::new(finalised_state::ZainoDB::spawn(config.clone(), source.clone()).await?);
        let mempool_state = mempool::Mempool::spawn(source.clone(), None)
            .map_err(crate::InitError::MempoolInitialzationError)
            .await?;

        let reader = finalized_db.to_reader();
        let top_of_finalized = if let Some(height) = reader.db_height().await? {
            reader.get_chain_block(height).await?
        } else {
            None
        };

        let non_finalized_state = crate::NonFinalizedState::initialize(
            source.clone(),
            config.network.to_zebra_network(),
            top_of_finalized,
        )
        .await?;

        let mut chain_index = Self {
            mempool: std::sync::Arc::new(mempool_state),
            non_finalized_state: std::sync::Arc::new(non_finalized_state),
            finalized_db,
            sync_loop_handle: None,
            status: AtomicStatus::new(StatusType::Spawning),
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
        }
    }

    /// Shut down the sync process, for a cleaner drop
    /// an error indicates a failure to cleanly shutdown. Dropping the
    /// chain index should still stop everything
    pub async fn shutdown(&self) -> Result<(), FinalisedStateError> {
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

    pub(super) fn start_sync_loop(&self) -> tokio::task::JoinHandle<Result<(), SyncError>> {
        info!("Starting ChainIndex sync.");
        let nfs = self.non_finalized_state.clone();
        let fs = self.finalized_db.clone();
        let status = self.status.clone();
        let source = self.non_finalized_state.source.clone();

        tokio::task::spawn(async move {
            let result: Result<(), SyncError> = async {
                loop {
                    if status.load() == StatusType::Closing {
                        break;
                    }

                    status.store(StatusType::Syncing);

                    // Sync fs to chain tip - 100.
                    let chain_height = source
                        .clone()
                        .get_best_block_height()
                        .await
                        .map_err(|error| {
                            SyncError::ValidatorConnectionError(
                                NodeConnectionError::UnrecoverableError(Box::new(error)),
                            )
                        })?
                        .ok_or_else(|| {
                            SyncError::ValidatorConnectionError(
                                NodeConnectionError::UnrecoverableError(Box::new(
                                    std::io::Error::other("node returned no best block height"),
                                )),
                            )
                        })?;
                    let finalised_height = crate::Height(chain_height.0.saturating_sub(100));

                    // TODO / FIX: Improve error handling here, fix and rebuild db on error.
                    fs.sync_to_height(finalised_height, &source)
                        .await
                        .map_err(|error| {
                            SyncError::ValidatorConnectionError(
                                NodeConnectionError::UnrecoverableError(Box::new(error)),
                            )
                        })?;

                    // Sync nfs to chain tip, trimming blocks to finalized tip.
                    nfs.sync(fs.clone()).await?;

                    status.store(StatusType::Ready);
                    // TODO: configure sleep duration?
                    tokio::time::sleep(Duration::from_millis(500)).await
                    // TODO: Check for shutdown signal.
                }
                Ok(())
            }
            .await;

            // If the sync loop exited unexpectedly with an error, set CriticalError
            // so that liveness checks can detect the failure.
            if let Err(ref e) = result {
                tracing::error!("Sync loop exited with error: {e:?}");
                status.store(StatusType::CriticalError);
            }

            result
        })
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
    non_finalized_state: std::sync::Arc<crate::NonFinalizedState<Source>>,
    finalized_state: finalised_state::reader::DbReader,
    status: AtomicStatus,
}

impl<Source: BlockchainSource> NodeBackedChainIndexSubscriber<Source> {
    fn source(&self) -> &Source {
        &self.non_finalized_state.source
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
                .map(|_| block.index().height())),
            None => self
                // ChainIndex step 4:
                .finalized_state
                .get_block_height(hash)
                .await
                .map_err(|_e| ChainIndexError::database_hole(hash)),
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
                    .get_chain_block(crate::Height(tx_location.block_height()))
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
        snapshot: &NonfinalizedBlockCacheSnapshot,
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
                        if height <= snapshot.validator_finalized_height {
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
    fn get_mempool_height(
        &self,
        snapshot: &NonfinalizedBlockCacheSnapshot,
    ) -> Option<types::Height> {
        snapshot
            .blocks
            .iter()
            .find(|(hash, _block)| **hash == self.mempool.mempool_chain_tip())
            .map(|(_hash, block)| block.height())
    }

    fn mempool_branch_id(&self, snapshot: &NonfinalizedBlockCacheSnapshot) -> Option<u32> {
        self.get_mempool_height(snapshot).and_then(|height| {
            ConsensusBranchId::current(
                &self.non_finalized_state.network,
                zebra_chain::block::Height::from(height + 1),
            )
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
    type Snapshot = Arc<NonfinalizedBlockCacheSnapshot>;
    type Error = ChainIndexError;

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot {
        self.non_finalized_state.get_snapshot()
    }

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
        match self.get_indexed_block_height(snapshot, hash).await? {
            Some(h) => Ok(Some(h)),
            None => self.get_block_height_passthrough(snapshot, hash).await, // ChainIndex step 5
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

        // We can serve blocks above where the validator has finalized
        // only if we have those blocks in our nonfinalized snapshot
        let max_servable_height = snapshot
            .validator_finalized_height
            .max(snapshot.best_tip.height);
        // The lower of the end of the provided range, and the highest block we can serve
        let end = end.unwrap_or(max_servable_height).min(max_servable_height);
        // Serve as high as we can, or to the provided end if it's lower
        if start <= max_servable_height.min(end) {
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
                                .ok_or(ChainIndexError::database_hole(hash))
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
                                        .ok_or(ChainIndexError::database_hole(block.hash()))
                                }
                                None => self
                                    // usually getting by height is not reorg-safe, but here, height is known to be below or equal to validator_finalized_height.
                                    .get_fullblock_bytes_from_node(HashOrHeight::Height(
                                        zebra_chain::block::Height(height),
                                    ))
                                    .await?
                                    .ok_or(ChainIndexError::database_hole(height)),
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
    async fn get_compact_block(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error> {
        if height <= nonfinalized_snapshot.best_tip.height {
            Ok(Some(
                match nonfinalized_snapshot.get_chainblock_by_height(&height) {
                    Some(block) => compact_block_with_pool_types(
                        block.to_compact_block(),
                        &pool_types.to_pool_types_vector(),
                    ),
                    None => match self
                        .finalized_state
                        .get_compact_block(height, pool_types)
                        .await
                    {
                        Ok(block) => block,
                        Err(_e) => return Err(ChainIndexError::database_hole(height)),
                    },
                },
            ))
        } else {
            Ok(None)
        }
    }

    /// Streams *compact* blocks for an inclusive height range.
    ///
    /// Returns `None` if either requested height is greater than the snapshot's tip.
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
    async fn get_compact_block_stream(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, Self::Error> {
        let chain_tip_height = nonfinalized_snapshot.best_chaintip().height;

        if start_height > chain_tip_height || end_height > chain_tip_height {
            return Ok(None);
        }

        // The nonfinalized cache holds the tip block plus the previous 99 blocks (100 total),
        // so the lowest possible cached height is `tip - 99` (saturating at 0).
        let lowest_nonfinalized_height = types::Height(chain_tip_height.0.saturating_sub(99));

        let is_ascending = start_height <= end_height;

        let pool_types_vector = pool_types.to_pool_types_vector();

        // Pre-create any finalized-state stream(s) we will need so that errors are returned
        // from this method (not deferred into the spawned task).
        let finalized_stream: Option<CompactBlockStream> = if is_ascending {
            if start_height < lowest_nonfinalized_height {
                let finalized_end_height = types::Height(std::cmp::min(
                    end_height.0,
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
        // TODO: Investigate whether channel size should be changed, added to config, or set dynamically base on resources.
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

                for height_value in nonfinalized_start_height.0..=end_height.0 {
                    let Some(indexed_block) = nonfinalized_snapshot
                        .get_chainblock_by_height(&types::Height(height_value))
                    else {
                        let _ = channel_sender
                        .send(Err(tonic::Status::internal(format!(
                            "Internal error, missing nonfinalized block at height [{height_value}].",
                        ))))
                        .await;
                        return;
                    };
                    let compact_block = compact_block_with_pool_types(
                        indexed_block.to_compact_block(),
                        &pool_types_vector,
                    );
                    if channel_sender.send(Ok(compact_block)).await.is_err() {
                        return;
                    }
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
                            let _ = channel_sender
                            .send(Err(tonic::Status::internal(format!(
                                "Internal error, missing nonfinalized block at height [{height_value}].",
                            ))))
                            .await;
                            return;
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

        // ChainIndex step 2:
        match snapshot.as_ref().get_chainblock_by_hash(hash) {
            Some(block) => {
                // At this point, we know that
                // The block is non-FINALIZED in the INDEXER
                // ChainIndex step 3:
                if snapshot.heights_to_hashes.get(&block.height()) == Some(block.hash()) {
                    // The block is in the best chain.
                    Ok(Some((*block.hash(), block.height())))
                } else {
                    // Otherwise, it's non-best chain! Grab its parent, and recurse
                    Box::pin(self.find_fork_point(snapshot, block.index().parent_hash())).await
                    // gotta pin recursive async functions to prevent infinite-sized
                    // Future-implementing types
                }
            }
            None => {
                // At this point, we know that
                // the block is NOT non-FINALIZED in the INDEXER.
                // ChainIndex step 4
                match self.finalized_state.get_block_height(*hash).await {
                    Ok(Some(height)) => {
                        // the block is FINALIZED in the INDEXER
                        Ok(Some((*hash, height)))
                    }
                    Err(_e) => Err(ChainIndexError::database_hole(hash)),
                    Ok(None) => {
                        // At this point, we know that
                        // the block is NOT FINALIZED in the INDEXER
                        // (NEITHER is it non-FINALIZED in the INDEXER)

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
                                        if height <= snapshot.validator_finalized_height {
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
                txid: txid.to_string(),
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
                match self
                    .blocks_containing_transaction(snapshot, txid.0)
                    .await?
                    .next()
                {
                    Some(block) => block.index.height.into(),
                    // If we don't have a block containing the transaction
                    // locally and the transaction's not on the validator's
                    // best chain, we can't determine its consensus branch ID
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
            ConsensusBranchId::current(&self.non_finalized_state.network, height).map(u32::from),
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
        let blocks_containing_transaction = self
            .blocks_containing_transaction(snapshot, txid.0)
            .await?
            .collect::<Vec<_>>();
        let Some(start_of_nonfinalized) = snapshot.heights_to_hashes.keys().min() else {
            return Err(ChainIndexError::database_hole("no blocks"));
        };
        let mut best_chain_block = blocks_containing_transaction
            .iter()
            .find(|block| {
                snapshot.heights_to_hashes.get(&block.height()) == Some(block.hash())
                    || block.height() < *start_of_nonfinalized
                // this block is either in the best chain ``heights_to_hashes`` or finalized.
            })
            .map(|block| BestChainLocation::Block(*block.hash(), block.height()));
        let mut non_best_chain_blocks: HashSet<NonBestChainLocation> =
            blocks_containing_transaction
                .iter()
                .filter(|block| {
                    snapshot.heights_to_hashes.get(&block.height()) != Some(block.hash())
                        && block.height() >= *start_of_nonfinalized
                })
                .map(|block| NonBestChainLocation::Block(*block.hash(), block.height()))
                .collect();
        let in_mempool = self
            .mempool
            .contains_txid(&mempool::MempoolKey {
                txid: txid.to_string(),
            })
            .await;
        if in_mempool {
            let mempool_tip_hash = self.mempool.mempool_chain_tip();
            if mempool_tip_hash == snapshot.best_tip.blockhash {
                if best_chain_block.is_some() {
                    return Err(ChainIndexError {
                        kind: ChainIndexErrorKind::InvalidSnapshot,
                        message:
                            "Best chain and up-to-date mempool both contain the same transaction"
                                .to_string(),
                        source: None,
                    });
                } else {
                    best_chain_block =
                        Some(BestChainLocation::Mempool(snapshot.best_tip.height + 1));
                }
            } else {
                // the best chain and the mempool have divergent tip hashes
                // get a new snapshot and use it to find the height of the mempool
                let target_height = self
                    .non_finalized_state
                    .get_snapshot()
                    .blocks
                    .iter()
                    .find_map(|(hash, block)| {
                        if *hash == mempool_tip_hash {
                            Some(block.height() + 1)
                            // found the block that is the tip that the mempool is hanging on to
                        } else {
                            None
                        }
                    });
                non_best_chain_blocks.insert(NonBestChainLocation::Mempool(target_height));
            }
        }

        // If we haven't found a block on the best chain,
        // try passthrough
        if best_chain_block.is_none() {
            if let Some((_transaction, GetTransactionLocation::BestChain(height))) = self
                .source()
                .get_transaction(*txid)
                .await
                .map_err(ChainIndexError::backing_validator)?
            {
                if height <= snapshot.validator_finalized_height {
                    if let Some(block) = self
                        .source()
                        .get_block(HashOrHeight::Height(height))
                        .await
                        .map_err(ChainIndexError::backing_validator)?
                    {
                        best_chain_block =
                            Some(BestChainLocation::Block(block.hash().into(), height.into()));
                    }
                }
            }
        }

        Ok((best_chain_block, non_best_chain_blocks))
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
        let expected_chain_tip = snapshot.map(|snapshot| snapshot.best_tip.blockhash);
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

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    async fn get_mempool_info(&self) -> MempoolInfo {
        self.mempool.get_mempool_info().await
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

    fn best_chaintip(&self) -> BestTip {
        self.as_ref().best_chaintip()
    }
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock>;
    /// Get the tip of the best chain, according to the snapshot
    fn best_chaintip(&self) -> BestTip;
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

    fn best_chaintip(&self) -> BestTip {
        self.best_tip
    }
}
