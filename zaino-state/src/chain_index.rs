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
use crate::chain_index::types::{BestChainLocation, NonBestChainLocation};
use crate::error::{ChainIndexError, ChainIndexErrorKind, FinalisedStateError};
use crate::IndexedBlock;
use crate::{AtomicStatus, StatusType, SyncError};
use std::collections::HashSet;
use std::{sync::Arc, time::Duration};

use futures::{FutureExt, Stream};
use non_finalised_state::NonfinalizedBlockCacheSnapshot;
use source::{BlockchainSource, ValidatorConnector};
use tokio_stream::StreamExt;
use tracing::info;
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
        nonfinalized_snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<types::Height>, Self::Error>>;

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified end height
    /// is greater than the snapshot's tip
    #[allow(clippy::type_complexity)]
    fn get_block_range(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start: types::Height,
        end: Option<types::Height>,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        block_hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error>;

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
    /// containing that transaction. Height is None for blocks not on the best chain.
    ///
    /// Also returns a bool representing whether the transaction is *currently* in the mempool.
    /// This is not currently tied to the given snapshot but rather uses the live mempool.
    #[allow(clippy::type_complexity)]
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), Self::Error>,
    >;

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
    /// If the chain tip has changed from the given spanshot returns None.
    #[allow(clippy::type_complexity)]
    fn get_mempool_stream(
        &self,
        snapshot: &Self::Snapshot,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;
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
pub struct NodeBackedChainIndex<Source: BlockchainSource = ValidatorConnector> {
    blockchain_source: std::sync::Arc<Source>,
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
            blockchain_source: Arc::new(source),
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
    pub async fn subscriber(&self) -> NodeBackedChainIndexSubscriber<Source> {
        NodeBackedChainIndexSubscriber {
            blockchain_source: self.blockchain_source.as_ref().clone(),
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
        self.finalized_db.shutdown().await?;
        self.mempool.close();
        self.status.store(StatusType::Closing);
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
        tokio::task::spawn(async move {
            loop {
                if status.load() == StatusType::Closing {
                    break;
                }

                status.store(StatusType::Syncing);
                // Sync nfs to chain tip, trimming blocks to finalized tip.
                nfs.sync(fs.clone()).await?;

                // Sync fs to chain tip - 100.
                {
                    let snapshot = nfs.get_snapshot();
                    while snapshot.best_tip.height.0
                        > (fs
                            .to_reader()
                            .db_height()
                            .await
                            .map_err(|_e| SyncError::CannotReadFinalizedState)?
                            .unwrap_or(types::Height(0))
                            .0
                            + 100)
                    {
                        let next_finalized_height = fs
                            .to_reader()
                            .db_height()
                            .await
                            .map_err(|_e| SyncError::CannotReadFinalizedState)?
                            .map(|height| height + 1)
                            .unwrap_or(types::Height(0));
                        let next_finalized_block = snapshot
                            .blocks
                            .get(
                                snapshot
                                    .heights_to_hashes
                                    .get(&(next_finalized_height))
                                    .ok_or(SyncError::CompetingSyncProcess)?,
                            )
                            .ok_or(SyncError::CompetingSyncProcess)?;
                        // TODO: Handle write errors better (fix db and continue)
                        fs.write_block(next_finalized_block.clone())
                            .await
                            .map_err(|_e| SyncError::CompetingSyncProcess)?;
                    }
                }
                status.store(StatusType::Ready);
                // TODO: configure sleep duration?
                tokio::time::sleep(Duration::from_millis(500)).await
                // TODO: Check for shutdown signal.
            }
            Ok(())
        })
    }
}

/// A clone-safe *read-only* view onto a running [`NodeBackedChainIndex`].
///
/// Designed for concurrent efficiency.
///
/// [`NodeBackedChainIndexSubscriber`] can safely be cloned and dropped freely.
#[derive(Clone)]
pub struct NodeBackedChainIndexSubscriber<Source: BlockchainSource = ValidatorConnector> {
    blockchain_source: Source,
    mempool: mempool::MempoolSubscriber,
    non_finalized_state: std::sync::Arc<crate::NonFinalizedState<Source>>,
    finalized_state: finalised_state::reader::DbReader,
    status: AtomicStatus,
}

impl<Source: BlockchainSource> NodeBackedChainIndexSubscriber<Source> {
    /// Displays the status of the chain_index
    pub fn status(&self) -> StatusType {
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
        self.non_finalized_state
            .source
            .get_block(id)
            .await
            .map_err(ChainIndexError::backing_validator)?
            .map(|bk| {
                bk.zcash_serialize_to_vec()
                    .map_err(ChainIndexError::backing_validator)
            })
            .transpose()
    }

    async fn blocks_containing_transaction<'snapshot, 'self_lt, 'iter>(
        &'self_lt self,
        snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> Result<impl Iterator<Item = IndexedBlock> + use<'iter, Source>, FinalisedStateError>
    where
        'snapshot: 'iter,
        'self_lt: 'iter,
    {
        Ok(snapshot
            .blocks
            .values()
            .filter_map(move |block| {
                block.transactions().iter().find_map(|transaction| {
                    if transaction.txid().0 == txid {
                        Some(block)
                    } else {
                        None
                    }
                })
            })
            .cloned()
            .chain(
                match self
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
                .into_iter(),
            ))
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
        nonfinalized_snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, Self::Error> {
        match nonfinalized_snapshot.blocks.get(&hash).cloned() {
            Some(block) => Ok(block.index().height()),
            None => match self.finalized_state.get_block_height(hash).await {
                Ok(height) => Ok(height),
                Err(_e) => Err(ChainIndexError::database_hole(hash)),
            },
        }
    }

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified end height
    /// is greater than the snapshot's tip
    fn get_block_range(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start: types::Height,
        end: std::option::Option<types::Height>,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let end = end.unwrap_or(nonfinalized_snapshot.best_tip.height);
        if end <= nonfinalized_snapshot.best_tip.height {
            Some(
                futures::stream::iter((start.0)..=(end.0)).then(move |height| async move {
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
                            match nonfinalized_snapshot
                                .get_chainblock_by_height(&types::Height(height))
                            {
                                Some(block) => {
                                    return self
                                        .get_fullblock_bytes_from_node(HashOrHeight::Hash(
                                            (*block.hash()).into(),
                                        ))
                                        .await?
                                        .ok_or(ChainIndexError::database_hole(block.hash()))
                                }
                                None => Err(ChainIndexError::database_hole(height)),
                            }
                        }
                    }
                }),
            )
        } else {
            None
        }
    }

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        block_hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error> {
        let Some(block) = snapshot.as_ref().get_chainblock_by_hash(block_hash) else {
            // No fork point found. This is not an error,
            // as zaino does not guarentee knowledge of all sidechain data.
            return Ok(None);
        };
        if let Some(height) = block.height() {
            Ok(Some((*block.hash(), height)))
        } else {
            self.find_fork_point(snapshot, block.index().parent_hash())
        }
    }

    /// Returns the block commitment tree data by hash
    async fn get_treestate(
        &self,
        // snapshot: &Self::Snapshot,
        // currently not implemented internally, fetches data from validator.
        //
        // NOTE: Should this check blockhash exists in snapshot and db before proxying call?
        hash: &types::BlockHash,
    ) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>), Self::Error> {
        match self.blockchain_source.get_treestate(*hash).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(ChainIndexError {
                kind: ChainIndexErrorKind::InternalServerError,
                message: "failed to fetch treestate from validator".to_string(),
                source: Some(Box::new(e)),
            }),
        }
    }

    /// given a transaction id, returns the transaction
    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, Self::Error> {
        if let Some(mempool_tx) = self
            .mempool
            .get_transaction(&mempool::MempoolKey {
                txid: txid.to_string(),
            })
            .await
        {
            let bytes = mempool_tx.serialized_tx.as_ref().as_ref().to_vec();
            let mempool_height = snapshot
                .blocks
                .iter()
                .find(|(hash, _block)| **hash == self.mempool.mempool_chain_tip())
                .and_then(|(_hash, block)| block.height());
            let mempool_branch_id = mempool_height.and_then(|height| {
                ConsensusBranchId::current(
                    &self.non_finalized_state.network,
                    zebra_chain::block::Height::from(height + 1),
                )
                .map(u32::from)
            });

            return Ok(Some((bytes, mempool_branch_id)));
        }

        let Some(block) = self
            .blocks_containing_transaction(snapshot, txid.0)
            .await?
            .next()
        else {
            return Ok(None);
        };

        // NOTE: Could we safely use zebra's get transaction method here without invalidating the snapshot?
        // This would be a more efficient way to fetch transaction data.
        //
        // Should NodeBackedChainIndex keep a clone of source to use here?
        //
        // This will require careful attention as there is a case where a transaction may still exist,
        // but may have been reorged into a different block, possibly breaking the validation of this interface.
        let full_block = self
            .non_finalized_state
            .source
            .get_block(HashOrHeight::Hash((block.index().hash().0).into()))
            .await
            .map_err(ChainIndexError::backing_validator)?
            .ok_or_else(|| ChainIndexError::database_hole(block.index().hash()))?;
        let block_consensus_branch_id = full_block.coinbase_height().and_then(|height| {
            ConsensusBranchId::current(&self.non_finalized_state.network, dbg!(height))
                .map(u32::from)
        });
        dbg!(block_consensus_branch_id);
        full_block
            .transactions
            .iter()
            .find(|transaction| {
                let txn_txid = transaction.hash().0;
                txn_txid == txid.0
            })
            .map(ZcashSerialize::zcash_serialize_to_vec)
            .ok_or_else(|| ChainIndexError::database_hole(block.index().hash()))?
            .map_err(ChainIndexError::backing_validator)
            .map(|transaction| (transaction, block_consensus_branch_id))
            .map(Some)
    }

    /// Given a transaction ID, returns all known blocks containing this transaction
    ///
    /// If the transaction is in the mempool, it will be in the BestChainLocation
    /// if the mempool and snapshot are up-to-date, and the NonBestChainLocation set
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
        let mut best_chain_block = blocks_containing_transaction
            .iter()
            .find_map(|block| BestChainLocation::try_from(block).ok());
        let mut non_best_chain_blocks: HashSet<NonBestChainLocation> =
            blocks_containing_transaction
                .iter()
                .filter_map(|block| NonBestChainLocation::try_from(block).ok())
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
                let target_height = self
                    .non_finalized_state
                    .get_snapshot()
                    .blocks
                    .iter()
                    .find_map(|(hash, block)| {
                        if *hash == mempool_tip_hash {
                            Some(block.height().map(|height| height + 1))
                        } else {
                            None
                        }
                    })
                    .flatten();
                non_best_chain_blocks.insert(NonBestChainLocation::Mempool(target_height));
            }
        }

        Ok((best_chain_block, non_best_chain_blocks))
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
        let subscriber = self.mempool.clone();

        // Use the mempool's own filtering (it already handles client-endian shortened prefixes).
        let pairs: Vec<(mempool::MempoolKey, mempool::MempoolValue)> =
            subscriber.get_filtered_mempool(exclude_list).await;

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
    /// Returns None if the chain tip has changed from the given snapshot.
    fn get_mempool_stream(
        &self,
        snapshot: &Self::Snapshot,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let expected_chain_tip = snapshot.best_tip.blockhash;
        let mut subscriber = self.mempool.clone();

        match subscriber
            .get_mempool_stream(Some(expected_chain_tip))
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
