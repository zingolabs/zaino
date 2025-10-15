use super::{finalised_state::ZainoDB, source::BlockchainSource};
use crate::{
    chain_index::types::{self, BlockHash, Height, TransactionHash, GENESIS_HEIGHT},
    error::FinalisedStateError,
    BlockData, BlockIndex, ChainWork, CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes,
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTxData, IndexedBlock,
    OrchardCompactTx, SaplingCompactTx, TransparentCompactTx, TxInCompact, TxOutCompact,
};
use arc_swap::ArcSwap;
use futures::lock::Mutex;
use primitive_types::U256;
use std::{collections::HashMap, mem, sync::Arc};
use tokio::sync::mpsc;
use tracing::{info, warn};
use zebra_chain::parameters::Network;
use zebra_state::HashOrHeight;

/// Holds the block cache
pub struct NonFinalizedState<Source: BlockchainSource> {
    /// We need access to the validator's best block hash, as well
    /// as a source of blocks
    pub(super) source: Source,
    staged: Mutex<mpsc::Receiver<IndexedBlock>>,
    staging_sender: mpsc::Sender<IndexedBlock>,
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

#[derive(Debug)]
/// A snapshot of the nonfinalized state as it existed when this was created.
pub struct NonfinalizedBlockCacheSnapshot {
    /// the set of all known blocks < 100 blocks old
    /// this includes all blocks on-chain, as well as
    /// all blocks known to have been on-chain before being
    /// removed by a reorg. Blocks reorged away have no height.
    pub blocks: HashMap<BlockHash, IndexedBlock>,
    /// hashes indexed by height
    pub heights_to_hashes: HashMap<Height, BlockHash>,
    // Do we need height here?
    /// The highest known block
    // best_tip is a BestTip, which contains
    // a Height, and a BlockHash as named fields.
    pub best_tip: BestTip,
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
/// An error occurred during sync of the NonFinalized State.
pub enum SyncError {
    /// The backing validator node returned corrupt, invalid, or incomplete data
    /// TODO: This may not be correctly disambibuated from temporary network issues
    /// in the fetchservice case.
    ZebradConnectionError(NodeConnectionError),
    /// The channel used to store new blocks has been closed. This should only happen
    /// during shutdown.
    StagingChannelClosed,
    /// Sync has been called multiple times in parallel, or another process has
    /// written to the block snapshot.
    CompetingSyncProcess,
    /// Sync attempted a reorg, and something went wrong. Currently, this
    /// only happens when we attempt to reorg below the start of the chain,
    /// indicating an entirely separate regtest/testnet chain to what we expected
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
        // TODO: Consider arbitrary buffer length
        info!("Initialising non-finalised state.");
        let (staging_sender, staging_receiver) = mpsc::channel(100);
        let staged = Mutex::new(staging_receiver);
        let chainblock = match start_block {
            Some(block) => block,
            None => {
                let genesis_block = source
                    .get_block(HashOrHeight::Height(zebra_chain::block::Height(0)))
                    .await
                    .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?
                    .ok_or_else(|| InitError::InvalidNodeData(Box::new(MissingGenesisBlock)))?;
                let (sapling_root_and_len, orchard_root_and_len) = source
                    .get_commitment_tree_roots(genesis_block.hash().into())
                    .await
                    .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?;
                let ((sapling_root, sapling_size), (orchard_root, orchard_size)) = (
                    sapling_root_and_len.unwrap_or_default(),
                    orchard_root_and_len.unwrap_or_default(),
                );

                let data = BlockData {
                    version: genesis_block.header.version,
                    time: genesis_block.header.time.timestamp(),
                    merkle_root: genesis_block.header.merkle_root.0,
                    bits: u32::from_be_bytes(
                        genesis_block
                            .header
                            .difficulty_threshold
                            .bytes_in_display_order(),
                    ),
                    block_commitments: match genesis_block
                        .commitment(&network)
                        .map_err(|e| InitError::InvalidNodeData(Box::new(e)))?
                    {
                        zebra_chain::block::Commitment::PreSaplingReserved(bytes) => bytes,
                        zebra_chain::block::Commitment::FinalSaplingRoot(root) => root.into(),
                        zebra_chain::block::Commitment::ChainHistoryActivationReserved => [0; 32],
                        zebra_chain::block::Commitment::ChainHistoryRoot(
                            chain_history_mmr_root_hash,
                        ) => chain_history_mmr_root_hash.bytes_in_serialized_order(),
                        zebra_chain::block::Commitment::ChainHistoryBlockTxAuthCommitment(
                            chain_history_block_tx_auth_commitment_hash,
                        ) => {
                            chain_history_block_tx_auth_commitment_hash.bytes_in_serialized_order()
                        }
                    },

                    nonce: *genesis_block.header.nonce,
                    solution: genesis_block.header.solution.into(),
                };

                let mut transactions = Vec::new();
                for (i, trnsctn) in genesis_block.transactions.iter().enumerate() {
                    let transparent = TransparentCompactTx::new(
                        trnsctn
                            .inputs()
                            .iter()
                            .filter_map(|input| {
                                input.outpoint().map(|outpoint| {
                                    TxInCompact::new(outpoint.hash.0, outpoint.index)
                                })
                            })
                            .collect(),
                        trnsctn
                            .outputs()
                            .iter()
                            .filter_map(|output| {
                                TxOutCompact::try_from((
                                    u64::from(output.value),
                                    output.lock_script.as_raw_bytes(),
                                ))
                                .ok()
                            })
                            .collect(),
                    );

                    let sapling = SaplingCompactTx::new(
                        Some(i64::from(trnsctn.sapling_value_balance().sapling_amount())),
                        trnsctn
                            .sapling_nullifiers()
                            .map(|nf| CompactSaplingSpend::new(*nf.0))
                            .collect(),
                        trnsctn
                            .sapling_outputs()
                            .map(|output| {
                                CompactSaplingOutput::new(
                                    output.cm_u.to_bytes(),
                                    <[u8; 32]>::from(output.ephemeral_key),
                                    // This unwrap is unnecessary, but to remove it one would need to write
                                    // a new array of [input[0], input[1]..] and enumerate all 52 elements
                                    //
                                    // This would be uglier than the unwrap
                                    <[u8; 580]>::from(output.enc_ciphertext)[..52]
                                        .try_into()
                                        .unwrap(),
                                )
                            })
                            .collect(),
                    );
                    let orchard = OrchardCompactTx::new(
                        Some(i64::from(trnsctn.orchard_value_balance().orchard_amount())),
                        trnsctn
                            .orchard_actions()
                            .map(|action| {
                                CompactOrchardAction::new(
                                    <[u8; 32]>::from(action.nullifier),
                                    <[u8; 32]>::from(action.cm_x),
                                    <[u8; 32]>::from(action.ephemeral_key),
                                    // This unwrap is unnecessary, but to remove it one would need to write
                                    // a new array of [input[0], input[1]..] and enumerate all 52 elements
                                    //
                                    // This would be uglier than the unwrap
                                    <[u8; 580]>::from(action.enc_ciphertext)[..52]
                                        .try_into()
                                        .unwrap(),
                                )
                            })
                            .collect(),
                    );

                    let txdata = CompactTxData::new(
                        i as u64,
                        TransactionHash(trnsctn.hash().0),
                        transparent,
                        sapling,
                        orchard,
                    );
                    transactions.push(txdata);
                }

                let height = Some(GENESIS_HEIGHT);
                let hash = BlockHash::from(genesis_block.hash());
                let parent_hash = BlockHash::from(genesis_block.header.previous_block_hash);
                let chainwork = ChainWork::from(U256::from(
                    genesis_block
                        .header
                        .difficulty_threshold
                        .to_work()
                        .ok_or_else(|| {
                            InitError::InvalidNodeData(Box::new(InvalidData(format!(
                                "Invalid work field of block {hash} {height:?}"
                            ))))
                        })?
                        .as_u128(),
                ));

                let index = BlockIndex {
                    hash,
                    parent_hash,
                    chainwork,
                    height,
                };

                //TODO: Is a default (zero) root correct?
                let commitment_tree_roots = CommitmentTreeRoots::new(
                    <[u8; 32]>::from(sapling_root),
                    <[u8; 32]>::from(orchard_root),
                );

                let commitment_tree_size =
                    CommitmentTreeSizes::new(sapling_size as u32, orchard_size as u32);

                let commitment_tree_data =
                    CommitmentTreeData::new(commitment_tree_roots, commitment_tree_size);

                IndexedBlock {
                    index,
                    data,
                    transactions,
                    commitment_tree_data,
                }
            }
        };
        let working_height = chainblock
            .height()
            .ok_or(InitError::InitalBlockMissingHeight)?;
        let best_tip = BestTip {
            height: working_height,
            blockhash: chainblock.index().hash,
        };

        let mut blocks = HashMap::new();
        let mut heights_to_hashes = HashMap::new();
        let hash = chainblock.index().hash;
        blocks.insert(hash, chainblock);
        heights_to_hashes.insert(working_height, hash);

        let current = ArcSwap::new(Arc::new(NonfinalizedBlockCacheSnapshot {
            blocks,
            heights_to_hashes,
            best_tip,
        }));

        let nfs_change_listener = source
            .nonfinalized_listener()
            .await
            .ok()
            .flatten()
            .map(Mutex::new);
        Ok(Self {
            source,
            staged,
            staging_sender,
            current,
            network,
            nfs_change_listener,
        })
    }

    /// sync to the top of the chain, trimming to the finalised tip.
    pub(super) async fn sync(&self, finalized_db: Arc<ZainoDB>) -> Result<(), SyncError> {
        let initial_state = self.get_snapshot();
        let mut new_blocks = Vec::new();
        let mut nonbest_blocks = HashMap::new();
        let mut best_tip = initial_state.best_tip;
        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541

        while let Some(block) = self
            .source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(
                u32::from(best_tip.height) + 1,
            )))
            .await
            .map_err(|e| {
                // TODO: Check error. Determine what kind of error to return, this may be recoverable
                SyncError::ZebradConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                    e,
                )))
            })?
        {
            // If this block is next in the chain, we sync it as normal
            let parent_hash = BlockHash::from(block.header.previous_block_hash);
            if parent_hash == best_tip.blockhash {
                let prev_block = match new_blocks.last() {
                    Some(block) => block,
                    None => initial_state
                        .blocks
                        .get(&best_tip.blockhash)
                        .ok_or_else(|| {
                            SyncError::ReorgFailure(format!(
                                "found blocks {:?}, expected block {:?}",
                                initial_state
                                    .blocks
                                    .values()
                                    .map(|block| (block.index().hash(), block.index().height()))
                                    .collect::<Vec<_>>(),
                                best_tip
                            ))
                        })?,
                };
                let chainblock = self.block_to_chainblock(prev_block, &block).await?;
                info!(
                    "syncing block {} at height {}",
                    &chainblock.index().hash(),
                    best_tip.height + 1
                );
                best_tip = BestTip {
                    height: best_tip.height + 1,
                    blockhash: *chainblock.hash(),
                };
                new_blocks.push(chainblock.clone());
            } else {
                info!("Reorg detected at height {}", best_tip.height + 1);
                let mut next_height_down = best_tip.height - 1;
                // If not, there's been a reorg, and we need to adjust our best-tip
                let prev_hash = loop {
                    if next_height_down == Height(0) {
                        return Err(SyncError::ReorgFailure(
                            "attempted to reorg below chain genesis".to_string(),
                        ));
                    }
                    match initial_state
                        .blocks
                        .values()
                        .find(|block| block.height() == Some(next_height_down))
                        .map(IndexedBlock::hash)
                    {
                        Some(hash) => break hash,
                        // There is a hole in our database.
                        // TODO: An error return may be more appropriate here
                        None => next_height_down = next_height_down - 1,
                    }
                };

                best_tip = BestTip {
                    height: next_height_down,
                    blockhash: *prev_hash,
                };
                // We can't calculate things like chainwork until we
                // know the parent block
                // this is done separately, after we've updated with the
                // best chain blocks
                nonbest_blocks.insert(block.hash(), block);
            }
        }

        for block in new_blocks {
            if let Err(e) = self
                .sync_stage_update_loop(block, finalized_db.clone())
                .await
            {
                return Err(e.into());
            }
        }

        if let Some(ref listener) = self.nfs_change_listener {
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
                            nonbest_blocks.insert(block.hash(), block);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(e @ mpsc::error::TryRecvError::Disconnected) => {
                        return Err(SyncError::ZebradConnectionError(
                            NodeConnectionError::UnrecoverableError(Box::new(e)),
                        ))
                    }
                }
            }
        }

        self.update(finalized_db.clone()).await?;

        let mut nonbest_chainblocks = HashMap::new();
        loop {
            let (next_up, later): (Vec<_>, Vec<_>) = nonbest_blocks
                .into_iter()
                .map(|(hash, block)| {
                    let prev_hash =
                        crate::chain_index::types::BlockHash(block.header.previous_block_hash.0);
                    (
                        hash,
                        block,
                        self.current
                            .load()
                            .blocks
                            .get(&prev_hash)
                            .or_else(|| nonbest_chainblocks.get(&prev_hash))
                            .cloned(),
                    )
                })
                .partition(|(_hash, _block, prev_block)| prev_block.is_some());

            if next_up.is_empty() {
                // Only store non-best chain blocks
                // if we have a path from them
                // to the chain
                break;
            }

            for (_hash, block, parent_block) in next_up {
                let chainblock = self
                    .block_to_chainblock(
                        &parent_block.expect("partitioned, known to be some"),
                        &block,
                    )
                    .await?;
                nonbest_chainblocks.insert(*chainblock.hash(), chainblock);
            }
            nonbest_blocks = later
                .into_iter()
                .map(|(hash, block, _parent_block)| (hash, block))
                .collect();
        }

        for block in nonbest_chainblocks.into_values() {
            if let Err(e) = self
                .sync_stage_update_loop(block, finalized_db.clone())
                .await
            {
                return Err(e.into());
            }
        }
        Ok(())
    }

    async fn sync_stage_update_loop(
        &self,
        block: IndexedBlock,
        finalized_db: Arc<ZainoDB>,
    ) -> Result<(), UpdateError> {
        if let Err(e) = self.stage(block.clone()) {
            match *e {
                mpsc::error::TrySendError::Full(_) => {
                    self.update(finalized_db.clone()).await?;
                    Box::pin(self.sync_stage_update_loop(block, finalized_db)).await?;
                }
                mpsc::error::TrySendError::Closed(_block) => {
                    return Err(UpdateError::ReceiverDisconnected)
                }
            }
        }
        Ok(())
    }

    /// Stage a block
    fn stage(
        &self,
        block: IndexedBlock,
    ) -> Result<(), Box<mpsc::error::TrySendError<IndexedBlock>>> {
        self.staging_sender.try_send(block).map_err(Box::new)
    }

    /// Add all blocks from the staging area, and save a new cache snapshot, trimming block below the finalised tip.
    async fn update(&self, finalized_db: Arc<ZainoDB>) -> Result<(), UpdateError> {
        let mut new = HashMap::<BlockHash, IndexedBlock>::new();
        let mut staged = self.staged.lock().await;
        loop {
            match staged.try_recv() {
                Ok(chain_block) => {
                    new.insert(*chain_block.index().hash(), chain_block);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(UpdateError::ReceiverDisconnected)
                }
            }
        }
        // at this point, we've collected everything in the staging area
        // we can drop the stage lock, and more blocks can be staged while we finish setting current
        mem::drop(staged);
        let snapshot = self.get_snapshot();
        new.extend(
            snapshot
                .blocks
                .iter()
                .map(|(hash, block)| (*hash, block.clone())),
        );

        let finalized_height = finalized_db
            .to_reader()
            .db_height()
            .await
            .map_err(|_e| UpdateError::FinalizedStateCorruption)?
            .unwrap_or(Height(0));

        let (_finalized_blocks, blocks): (HashMap<_, _>, HashMap<BlockHash, _>) = new
            .into_iter()
            .partition(|(_hash, block)| match block.index().height() {
                Some(height) => height < finalized_height,
                None => false,
            });

        let best_tip = blocks.iter().fold(snapshot.best_tip, |acc, (hash, block)| {
            match block.index().height() {
                Some(working_height) if working_height > acc.height => BestTip {
                    height: working_height,
                    blockhash: *hash,
                },
                _ => acc,
            }
        });

        let heights_to_hashes = blocks
            .iter()
            .filter_map(|(hash, chainblock)| {
                chainblock.index().height.map(|height| (height, *hash))
            })
            .collect();

        // Need to get best hash at some point in this process
        let stored = self.current.compare_and_swap(
            &snapshot,
            Arc::new(NonfinalizedBlockCacheSnapshot {
                blocks,
                heights_to_hashes,
                best_tip,
            }),
        );

        if Arc::ptr_eq(&stored, &snapshot) {
            let stale_best_tip = snapshot.best_tip;
            let new_best_tip = best_tip;

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
        let (sapling_root_and_len, orchard_root_and_len) = self
            .source
            .get_commitment_tree_roots(block.hash().into())
            .await
            .map_err(|e| {
                SyncError::ZebradConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                    e,
                )))
            })?;
        let ((sapling_root, sapling_size), (orchard_root, orchard_size)) = (
            sapling_root_and_len.unwrap_or_default(),
            orchard_root_and_len.unwrap_or_default(),
        );

        IndexedBlock::try_from((
            block,
            sapling_root,
            sapling_size as u32,
            orchard_root,
            orchard_size as u32,
            prev_block.chainwork(),
            &self.network,
        ))
        .map_err(|e| {
            SyncError::ZebradConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                InvalidData(e),
            )))
        })
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
}
