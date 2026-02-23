use super::{finalised_state::ZainoDB, source::BlockchainSource};
use crate::{
    chain_index::types::{self, BlockHash, BlockMetadata, BlockWithMetadata, Height, TreeRootData},
    error::FinalisedStateError,
    ChainWork, IndexedBlock,
};
use arc_swap::ArcSwap;
use futures::lock::Mutex;
use primitive_types::U256;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tracing::{info, warn};
use zebra_chain::{parameters::Network, serialization::BytesInDisplayOrder};
use zebra_state::HashOrHeight;

/// Holds the block cache
#[derive(Debug)]
pub struct NonFinalizedState<Source: BlockchainSource> {
    /// We need access to the validator's best block hash, as well
    /// as a source of blocks
    pub(super) source: Source,
    /// This lock should not be exposed to consumers. Rather,
    /// clone the Arc and offer that. This means we can overwrite the arc
    /// without interfering with readers, who will hold a stale copy
    current: ArcSwap<NonfinalizedBlockCacheSnapshot>,
    /// Used mostly to determine activation heights
    pub(crate) network: Network,
    /// Listener used to detect non-best-chain blocks, if available
    #[allow(clippy::type_complexity)]
    nfs_change_listener: Option<
        Mutex<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
    >,
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// created for NonfinalizedBlockCacheSnapshot best_tip field for naming fields
pub struct BestTip {
    /// from chain_index types
    pub height: Height,
    /// from chain_index types
    pub blockhash: BlockHash,
}

#[derive(Debug, Clone)]
/// A snapshot of the nonfinalized state as it existed when this was created.
pub struct NonfinalizedBlockCacheSnapshot {
    /// the set of all known blocks < 100 blocks old
    /// this includes all blocks on-chain, as well as
    /// all blocks known to have been on-chain before being
    /// removed by a reorg. Blocks reorged away have no height.
    pub blocks: HashMap<BlockHash, IndexedBlock>,
    /// hashes indexed by height
    /// Hashes in this map are part of the best chain.
    pub heights_to_hashes: HashMap<Height, BlockHash>,
    // Do we need height here?
    /// The highest known block
    // best_tip is a BestTip, which contains
    // a Height, and a BlockHash as named fields.
    pub best_tip: BestTip,

    /// if the validator has finalized above the tip
    /// of the snapshot, we can use it for some queries
    /// and pass through to the validator
    pub validator_finalized_height: Height,
}

#[derive(Debug)]
/// Could not connect to a validator
pub enum NodeConnectionError {
    /// The Uri provided was invalid
    BadUri(String),
    /// Could not connect to the zebrad.
    /// This is a network issue.
    ConnectionFailure(reqwest::Error),
    /// The Zebrad provided invalid or corrupt data. Something has gone wrong
    /// and we need to shut down.
    UnrecoverableError(Box<dyn std::error::Error + Send>),
}

#[derive(Debug)]
struct MissingBlockError(String);

impl std::fmt::Display for MissingBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing block: {}", self.0)
    }
}

impl std::error::Error for MissingBlockError {}

#[derive(Debug)]
/// An error occurred during sync of the NonFinalized State.
pub enum SyncError {
    /// The backing validator node returned corrupt, invalid, or incomplete data
    /// TODO: This may not be correctly disambibuated from temporary network issues
    /// in the fetchservice case.
    ValidatorConnectionError(NodeConnectionError),
    /// The channel used to store new blocks has been closed. This should only happen
    /// during shutdown.
    StagingChannelClosed,
    /// Sync has been called multiple times in parallel, or another process has
    /// written to the block snapshot.
    CompetingSyncProcess,
    /// Sync attempted a reorg, and something went wrong.
    ReorgFailure(String),
    /// UnrecoverableFinalizedStateError
    CannotReadFinalizedState,
}

impl From<UpdateError> for SyncError {
    fn from(value: UpdateError) -> Self {
        match value {
            UpdateError::ReceiverDisconnected => SyncError::StagingChannelClosed,
            UpdateError::StaleSnapshot => SyncError::CompetingSyncProcess,
            UpdateError::FinalizedStateCorruption => SyncError::CannotReadFinalizedState,
            UpdateError::DatabaseHole => {
                SyncError::ReorgFailure(String::from("could not determine best chain"))
            }
            UpdateError::ValidatorConnectionError(e) => SyncError::ValidatorConnectionError(
                NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(e.to_string()))),
            ),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("Genesis block missing in validator")]
struct MissingGenesisBlock;

#[derive(thiserror::Error, Debug)]
#[error("data from validator invalid: {0}")]
struct InvalidData(String);

#[derive(Debug, thiserror::Error)]
/// An error occured during initial creation of the NonFinalizedState
pub enum InitError {
    #[error("zebra returned invalid data: {0}")]
    /// the connected node returned garbage data
    InvalidNodeData(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    /// The mempool state failed to initialize
    MempoolInitialzationError(#[from] crate::error::MempoolError),
    #[error(transparent)]
    /// The finalized state failed to initialize
    FinalisedStateInitialzationError(#[from] FinalisedStateError),
    /// the initial block provided was not on the best chain
    #[error("initial block not on best chain")]
    InitalBlockMissingHeight,
}

/// This is the core of the concurrent block cache.
impl BestTip {
    /// Create a BestTip from an IndexedBlock
    fn from_block(block: &IndexedBlock) -> Result<Self, InitError> {
        let height = block.height();
        let blockhash = *block.hash();
        Ok(Self { height, blockhash })
    }
}

impl NonfinalizedBlockCacheSnapshot {
    /// Create initial snapshot from a single block
    fn from_initial_block(
        block: IndexedBlock,
        validator_finalized_height: Height,
    ) -> Result<Self, InitError> {
        let best_tip = BestTip::from_block(&block)?;
        let hash = *block.hash();
        let height = best_tip.height;

        let mut blocks = HashMap::new();
        let mut heights_to_hashes = HashMap::new();

        blocks.insert(hash, block);
        heights_to_hashes.insert(height, hash);

        Ok(Self {
            blocks,
            heights_to_hashes,
            best_tip,
            validator_finalized_height,
        })
    }

    fn add_block_new_chaintip(&mut self, block: IndexedBlock) {
        self.best_tip = BestTip {
            height: block.height(),
            blockhash: *block.hash(),
        };
        self.add_block(block)
    }

    fn get_block_by_hash_bytes_in_serialized_order(&self, hash: [u8; 32]) -> Option<&IndexedBlock> {
        self.blocks
            .values()
            .find(|block| block.hash_bytes_serialized_order() == hash)
    }

    fn remove_finalized_blocks(&mut self, finalized_height: Height) {
        // Keep the last finalized block. This means we don't have to check
        // the finalized state when the entire non-finalized state is reorged away.
        self.blocks
            .retain(|_hash, block| block.height() >= finalized_height);
        self.heights_to_hashes
            .retain(|height, _hash| height >= &finalized_height);
    }

    fn add_block(&mut self, block: IndexedBlock) {
        self.heights_to_hashes.insert(block.height(), *block.hash());
        self.blocks.insert(*block.hash(), block);
    }
}

impl<Source: BlockchainSource> NonFinalizedState<Source> {
    /// Create a nonfinalized state, in a coherent initial state
    ///
    /// TODO: Currently, we can't initate without an snapshot, we need to create a cache
    /// of at least one block. Should this be tied to the instantiation of the data structure
    /// itself?
    pub async fn initialize(
        source: Source,
        network: Network,
        start_block: Option<IndexedBlock>,
    ) -> Result<Self, InitError> {
        info!("Initialising non-finalised state.");

        let validator_tip = source
            .get_best_block_height()
            .await
            .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?
            .ok_or_else(|| {
                InitError::InvalidNodeData(Box::new(MissingBlockError(
                    "Validator has no best block".to_string(),
                )))
            })?;

        // Resolve the initial block (provided or genesis)
        let initial_block = Self::resolve_initial_block(&source, &network, start_block).await?;

        // Create initial snapshot from the block
        let snapshot = NonfinalizedBlockCacheSnapshot::from_initial_block(
            initial_block,
            Height(validator_tip.0.saturating_sub(100)),
        )?;

        // Set up optional listener
        let nfs_change_listener = Self::setup_listener(&source).await;

        Ok(Self {
            source,
            current: ArcSwap::new(Arc::new(snapshot)),
            network,
            nfs_change_listener,
        })
    }

    /// Fetch the genesis block and convert it to IndexedBlock
    async fn get_genesis_indexed_block(
        source: &Source,
        network: &Network,
    ) -> Result<IndexedBlock, InitError> {
        let genesis_block = source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(0)))
            .await
            .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?
            .ok_or_else(|| InitError::InvalidNodeData(Box::new(MissingGenesisBlock)))?;

        let (sapling_root_and_len, orchard_root_and_len) = source
            .get_commitment_tree_roots(genesis_block.hash().into())
            .await
            .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?;

        let tree_roots = TreeRootData {
            sapling: sapling_root_and_len,
            orchard: orchard_root_and_len,
        };

        // For genesis block, chainwork is just the block's own work (no previous blocks)
        let genesis_work = ChainWork::from(U256::from(
            genesis_block
                .header
                .difficulty_threshold
                .to_work()
                .ok_or_else(|| {
                    InitError::InvalidNodeData(Box::new(InvalidData(
                        "Invalid work field of genesis block".to_string(),
                    )))
                })?
                .as_u128(),
        ));

        Self::create_indexed_block_with_optional_roots(
            genesis_block.as_ref(),
            &tree_roots,
            genesis_work,
            network.clone(),
        )
        .map_err(|e| InitError::InvalidNodeData(Box::new(InvalidData(e))))
    }

    /// Resolve the initial block - either use provided block or fetch genesis
    async fn resolve_initial_block(
        source: &Source,
        network: &Network,
        start_block: Option<IndexedBlock>,
    ) -> Result<IndexedBlock, InitError> {
        match start_block {
            Some(block) => Ok(block),
            None => Self::get_genesis_indexed_block(source, network).await,
        }
    }

    /// Set up the optional non-finalized change listener
    async fn setup_listener(
        source: &Source,
    ) -> Option<
        Mutex<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
    > {
        source
            .nonfinalized_listener()
            .await
            .ok()
            .flatten()
            .map(Mutex::new)
    }

    /// sync to the top of the chain, trimming to the finalised tip.
    pub(super) async fn sync(&self, finalized_db: Arc<ZainoDB>) -> Result<(), SyncError> {
        let mut initial_state = self.get_snapshot();
        let mut working_snapshot = initial_state.as_ref().clone();

        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541

        while let Some(block) = self
            .source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(
                u32::from(working_snapshot.best_tip.height) + 1,
            )))
            .await
            .map_err(|e| {
                // TODO: Check error. Determine what kind of error to return, this may be recoverable
                SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(
                    Box::new(e),
                ))
            })?
        {
            let parent_hash = BlockHash::from(block.header.previous_block_hash);
            if parent_hash == working_snapshot.best_tip.blockhash {
                // Normal chain progression
                let prev_block = working_snapshot
                    .blocks
                    .get(&working_snapshot.best_tip.blockhash)
                    .ok_or_else(|| {
                        SyncError::ReorgFailure(format!(
                            "found blocks {:?}, expected block {:?}",
                            working_snapshot
                                .blocks
                                .values()
                                .map(|block| (block.index().hash(), block.index().height()))
                                .collect::<Vec<_>>(),
                            working_snapshot.best_tip
                        ))
                    })?;
                let chainblock = self.block_to_chainblock(prev_block, &block).await?;
                info!(
                    "syncing block {} at height {}",
                    &chainblock.index().hash(),
                    working_snapshot.best_tip.height + 1
                );
                working_snapshot.add_block_new_chaintip(chainblock);
            } else {
                self.handle_reorg(&mut working_snapshot, block.as_ref())
                    .await?;
                // There's been a reorg. The fresh block is the new chaintip
                // we need to work backwards from it and update heights_to_hashes
                // with it and all its parents.
            }
            if initial_state.best_tip.height + 100 < working_snapshot.best_tip.height {
                self.update(finalized_db.clone(), initial_state, working_snapshot)
                    .await?;
                initial_state = self.current.load_full();
                working_snapshot = initial_state.as_ref().clone();
            }
        }
        // Handle non-finalized change listener
        self.handle_nfs_change_listener(&mut working_snapshot)
            .await?;

        self.update(finalized_db.clone(), initial_state, working_snapshot)
            .await?;

        Ok(())
    }

    /// Handle a blockchain reorg by finding the common ancestor
    async fn handle_reorg(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
        block: &impl Block,
    ) -> Result<IndexedBlock, SyncError> {
        let prev_block = match working_snapshot
            .get_block_by_hash_bytes_in_serialized_order(block.prev_hash_bytes_serialized_order())
            .cloned()
        {
            Some(prev_block) => {
                if !working_snapshot
                    .heights_to_hashes
                    .values()
                    .any(|hash| hash == prev_block.hash())
                {
                    Box::pin(self.handle_reorg(working_snapshot, &prev_block)).await?
                } else {
                    prev_block
                }
            }
            None => {
                let prev_block = self
                    .source
                    .get_block(HashOrHeight::Hash(
                        zebra_chain::block::Hash::from_bytes_in_serialized_order(
                            block.prev_hash_bytes_serialized_order(),
                        ),
                    ))
                    .await
                    .map_err(|e| {
                        SyncError::ValidatorConnectionError(
                            NodeConnectionError::UnrecoverableError(Box::new(e)),
                        )
                    })?
                    .ok_or(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(
                            "zebrad missing block in best chain".to_string(),
                        ))),
                    ))?;
                Box::pin(self.handle_reorg(working_snapshot, &*prev_block)).await?
            }
        };
        let indexed_block = block.to_indexed_block(&prev_block, self).await?;
        working_snapshot.add_block_new_chaintip(indexed_block.clone());
        Ok(indexed_block)
    }

    /// Handle non-finalized change listener events
    async fn handle_nfs_change_listener(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
    ) -> Result<(), SyncError> {
        let Some(ref listener) = self.nfs_change_listener else {
            return Ok(());
        };

        let Some(mut listener) = listener.try_lock() else {
            warn!("Error fetching non-finalized change listener");
            return Err(SyncError::CompetingSyncProcess);
        };

        loop {
            match listener.try_recv() {
                Ok((hash, block)) => {
                    if !self
                        .current
                        .load()
                        .blocks
                        .contains_key(&types::BlockHash(hash.0))
                    {
                        self.add_nonbest_block(working_snapshot, &*block).await?;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(e @ mpsc::error::TryRecvError::Disconnected) => {
                    return Err(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(e)),
                    ))
                }
            }
        }
        Ok(())
    }

    /// Add all blocks from the staging area, and save a new cache snapshot, trimming block below the finalised tip.
    pub(super) async fn update(
        &self,
        finalized_db: Arc<ZainoDB>,
        initial_state: Arc<NonfinalizedBlockCacheSnapshot>,
        mut new_snapshot: NonfinalizedBlockCacheSnapshot,
    ) -> Result<(), UpdateError> {
        let finalized_height = finalized_db
            .to_reader()
            .db_height()
            .await
            .map_err(|_e| UpdateError::FinalizedStateCorruption)?
            .unwrap_or(Height(0));

        new_snapshot.remove_finalized_blocks(finalized_height);
        let best_block = &new_snapshot
            .blocks
            .values()
            .max_by_key(|block| block.chainwork())
            .cloned()
            .expect("empty snapshot impossible");
        self.handle_reorg(&mut new_snapshot, best_block)
            .await
            .map_err(|_e| UpdateError::DatabaseHole)?;

        let validator_tip = self
            .source
            .get_best_block_height()
            .await
            .map_err(|e| UpdateError::ValidatorConnectionError(Box::new(e)))?
            .ok_or(UpdateError::ValidatorConnectionError(Box::new(
                MissingBlockError("no best block height".to_string()),
            )))?;
        new_snapshot.validator_finalized_height = Height(validator_tip.0.saturating_sub(100));

        // Need to get best hash at some point in this process
        let stored = self
            .current
            .compare_and_swap(&initial_state, Arc::new(new_snapshot));

        if Arc::ptr_eq(&stored, &initial_state) {
            let stale_best_tip = initial_state.best_tip;
            let new_best_tip = stored.best_tip;

            // Log chain tip change
            if new_best_tip != stale_best_tip {
                if new_best_tip.height > stale_best_tip.height {
                    info!(
                        "non-finalized tip advanced: Height: {} -> {}, Hash: {} -> {}",
                        stale_best_tip.height,
                        new_best_tip.height,
                        stale_best_tip.blockhash,
                        new_best_tip.blockhash,
                    );
                } else if new_best_tip.height == stale_best_tip.height
                    && new_best_tip.blockhash != stale_best_tip.blockhash
                {
                    info!(
                        "non-finalized tip reorg at height {}: Hash: {} -> {}",
                        new_best_tip.height, stale_best_tip.blockhash, new_best_tip.blockhash,
                    );
                } else if new_best_tip.height < stale_best_tip.height {
                    info!(
                        "non-finalized tip rollback from height {} to {}, Hash: {} -> {}",
                        stale_best_tip.height,
                        new_best_tip.height,
                        stale_best_tip.blockhash,
                        new_best_tip.blockhash,
                    );
                }
            }
            Ok(())
        } else {
            Err(UpdateError::StaleSnapshot)
        }
    }

    /// Get a snapshot of the block cache
    pub(super) fn get_snapshot(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.current.load_full()
    }

    async fn block_to_chainblock(
        &self,
        prev_block: &IndexedBlock,
        block: &zebra_chain::block::Block,
    ) -> Result<IndexedBlock, SyncError> {
        let tree_roots = self
            .get_tree_roots_from_source(block.hash().into())
            .await
            .map_err(|e| {
                SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(
                    Box::new(InvalidData(format!("{}", e))),
                ))
            })?;

        Self::create_indexed_block_with_optional_roots(
            block,
            &tree_roots,
            *prev_block.chainwork(),
            self.network.clone(),
        )
        .map_err(|e| {
            SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                InvalidData(e),
            )))
        })
    }

    /// Get commitment tree roots from the blockchain source
    async fn get_tree_roots_from_source(
        &self,
        block_hash: BlockHash,
    ) -> Result<TreeRootData, super::source::BlockchainSourceError> {
        let (sapling_root_and_len, orchard_root_and_len) =
            self.source.get_commitment_tree_roots(block_hash).await?;

        Ok(TreeRootData {
            sapling: sapling_root_and_len,
            orchard: orchard_root_and_len,
        })
    }

    /// Create IndexedBlock with optional tree roots (for genesis/sync cases)
    ///
    /// TODO: Issue #604 - This uses `unwrap_or_default()` uniformly for both Sapling and Orchard,
    /// but they have different activation heights. This masks potential bugs and prevents proper
    /// validation based on network upgrade activation.
    fn create_indexed_block_with_optional_roots(
        block: &zebra_chain::block::Block,
        tree_roots: &TreeRootData,
        parent_chainwork: ChainWork,
        network: Network,
    ) -> Result<IndexedBlock, String> {
        let (sapling_root, sapling_size, orchard_root, orchard_size) =
            tree_roots.clone().extract_with_defaults();

        let metadata = BlockMetadata::new(
            sapling_root,
            sapling_size as u32,
            orchard_root,
            orchard_size as u32,
            parent_chainwork,
            network,
        );

        let block_with_metadata = BlockWithMetadata::new(block, metadata);
        IndexedBlock::try_from(block_with_metadata)
    }

    async fn add_nonbest_block(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
        block: &impl Block,
    ) -> Result<IndexedBlock, SyncError> {
        let prev_block = match working_snapshot
            .get_block_by_hash_bytes_in_serialized_order(block.prev_hash_bytes_serialized_order())
            .cloned()
        {
            Some(block) => block,
            None => {
                let prev_block = self
                    .source
                    .get_block(HashOrHeight::Hash(
                        zebra_chain::block::Hash::from_bytes_in_serialized_order(
                            block.prev_hash_bytes_serialized_order(),
                        ),
                    ))
                    .await
                    .map_err(|e| {
                        SyncError::ValidatorConnectionError(
                            NodeConnectionError::UnrecoverableError(Box::new(e)),
                        )
                    })?
                    .ok_or(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(
                            "zebrad missing block".to_string(),
                        ))),
                    ))?;
                Box::pin(self.add_nonbest_block(working_snapshot, &*prev_block)).await?
            }
        };
        let indexed_block = block.to_indexed_block(&prev_block, self).await?;
        working_snapshot.add_block(indexed_block.clone());
        Ok(indexed_block)
    }
}

/// Errors that occur during a snapshot update
pub enum UpdateError {
    /// The block reciever disconnected. This should only happen during shutdown.
    ReceiverDisconnected,
    /// The snapshot was already updated by a different process, between when this update started
    /// and when it completed.
    StaleSnapshot,

    /// Something has gone unrecoverably wrong in the finalized
    /// state. A full rebuild is likely needed
    FinalizedStateCorruption,

    /// A block in the snapshot is missing
    DatabaseHole,

    /// Failed to connect to the backing validator
    ValidatorConnectionError(Box<dyn std::error::Error>),
}

trait Block {
    fn hash_bytes_serialized_order(&self) -> [u8; 32];
    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32];
    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        prev_block: &IndexedBlock,
        nfs: &NonFinalizedState<Source>,
    ) -> Result<IndexedBlock, SyncError>;
}

impl Block for IndexedBlock {
    fn hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.hash().0
    }

    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.index.parent_hash.0
    }

    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        _prev_block: &IndexedBlock,
        _nfs: &NonFinalizedState<Source>,
    ) -> Result<IndexedBlock, SyncError> {
        Ok(self.clone())
    }
}
impl Block for zebra_chain::block::Block {
    fn hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.hash().bytes_in_serialized_order()
    }

    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.header.previous_block_hash.bytes_in_serialized_order()
    }

    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        prev_block: &IndexedBlock,
        nfs: &NonFinalizedState<Source>,
    ) -> Result<IndexedBlock, SyncError> {
        nfs.block_to_chainblock(prev_block, self).await
    }
}
