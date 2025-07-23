use std::{collections::HashMap, mem, sync::Arc};

use crate::{
    chain_index::types::{Hash, Height},
    BlockData, BlockIndex, ChainWork, CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes,
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTxData,
    OrchardCompactTx, SaplingCompactTx, TransparentCompactTx, TxInCompact, TxOutCompact,
};
use arc_swap::ArcSwap;
use futures::{future::join, lock::Mutex};
use primitive_types::U256;
use tokio::sync::mpsc;
use tower::Service;
use zaino_fetch::jsonrpsee::{
    connector::{JsonRpSeeConnector, RpcRequestError},
    response::{GetBlockError, GetBlockResponse, GetTreestateResponse},
};
use zcash_primitives::merkle_tree::read_commitment_tree;
use zebra_chain::{parameters::Network, serialization::ZcashDeserialize};
use zebra_state::{HashOrHeight, ReadResponse, ReadStateService};

use crate::ChainBlock;

/// Holds the block cache
pub struct NonFinalizedState {
    /// We need access to the validator's best block hash, as well
    /// as a source of blocks
    pub(super) source: BlockchainSource,
    staged: Mutex<mpsc::Receiver<ChainBlock>>,
    staging_sender: mpsc::Sender<ChainBlock>,
    /// This lock should not be exposed to consumers. Rather,
    /// clone the Arc and offer that. This means we can overwrite the arc
    /// without interfering with readers, who will hold a stale copy
    current: ArcSwap<NonfinalizedBlockCacheSnapshot>,
    /// Used mostly to determine activation heights
    network: Network,
}

#[derive(Debug)]
/// A snapshot of the nonfinalized state as it existed when this was created.
pub struct NonfinalizedBlockCacheSnapshot {
    /// the set of all known blocks < 100 blocks old
    /// this includes all blocks on-chain, as well as
    /// all blocks known to have been on-chain before being
    /// removed by a reorg. Blocks reorged away have no height.
    pub blocks: HashMap<Hash, ChainBlock>,
    /// hashes indexed by height
    pub heights_to_hashes: HashMap<Height, Hash>,
    // Do we need height here?
    /// The highest known block
    pub best_tip: (Height, Hash),
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
}

impl From<UpdateError> for SyncError {
    fn from(value: UpdateError) -> Self {
        match value {
            UpdateError::ReceiverDisconnected => SyncError::StagingChannelClosed,
            UpdateError::StaleSnapshot => SyncError::CompetingSyncProcess,
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
}

/// This is the core of the concurrent block cache.
impl NonFinalizedState {
    /// Create a nonfinalized state, in a coherent initial state
    ///
    /// TODO: Currently, we can't initate without an snapshot, we need to create a cache
    /// of at least one block. Should this be tied to the instantiation of the data structure
    /// itself?
    pub async fn initialize(source: BlockchainSource, network: Network) -> Result<Self, InitError> {
        // TODO: Consider arbitrary buffer length
        let (staging_sender, staging_receiver) = mpsc::channel(100);
        let staged = Mutex::new(staging_receiver);
        // The comment on the Height type is somewhat unclear if genesis block is height 0 or height 1
        // TODO: Confirm
        let genesis_block = source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(1)))
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
                zebra_chain::block::Commitment::ChainHistoryRoot(chain_history_mmr_root_hash) => {
                    chain_history_mmr_root_hash.bytes_in_serialized_order()
                }
                zebra_chain::block::Commitment::ChainHistoryBlockTxAuthCommitment(
                    chain_history_block_tx_auth_commitment_hash,
                ) => chain_history_block_tx_auth_commitment_hash.bytes_in_serialized_order(),
            },

            nonse: *genesis_block.header.nonce,
            solution: genesis_block.header.solution.into(),
        };

        let mut transactions = Vec::new();
        for (i, trnsctn) in genesis_block.transactions.iter().enumerate() {
            let transparent = TransparentCompactTx::new(
                trnsctn
                    .inputs()
                    .iter()
                    .filter_map(|input| {
                        input
                            .outpoint()
                            .map(|outpoint| TxInCompact::new(outpoint.hash.0, outpoint.index))
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

            let txdata =
                CompactTxData::new(i as u64, trnsctn.hash().0, transparent, sapling, orchard);
            transactions.push(txdata);
        }

        let height = Some(Height(1));
        let hash = Hash::from(genesis_block.hash());
        let parent_hash = Hash::from(genesis_block.header.previous_block_hash);
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

        let chainblock = ChainBlock {
            index,
            data,
            transactions,
            commitment_tree_data,
        };
        let best_tip = (Height(1), chainblock.index().hash);

        let mut blocks = HashMap::new();
        let mut heights_to_hashes = HashMap::new();
        let hash = chainblock.index().hash;
        blocks.insert(hash, chainblock);
        heights_to_hashes.insert(Height(1), hash);

        let current = ArcSwap::new(Arc::new(NonfinalizedBlockCacheSnapshot {
            blocks,
            heights_to_hashes,
            best_tip,
        }));
        Ok(Self {
            source,
            staged,
            staging_sender,
            current,
            network,
        })
    }

    /// sync to the top of the chain
    pub async fn sync(&self) -> Result<(), SyncError> {
        let initial_state = self.get_snapshot();
        let mut new_blocks = Vec::new();
        let mut sidechain_blocks = Vec::new();
        let mut best_tip = initial_state.best_tip;
        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541

        while let Some(block) = self
            .source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(
                u32::from(best_tip.0) + 1,
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
            if Hash::from(block.header.previous_block_hash) == best_tip.1 {
                let prev_block = match new_blocks.last() {
                    Some(block) => block,
                    None => initial_state.blocks.get(&best_tip.1).ok_or_else(|| {
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
                let (sapling_root_and_len, orchard_root_and_len) = self
                    .source
                    .get_commitment_tree_roots(block.hash().into())
                    .await
                    .map_err(|e| {
                        SyncError::ZebradConnectionError(NodeConnectionError::UnrecoverableError(
                            Box::new(e),
                        ))
                    })?;
                let ((sapling_root, sapling_size), (orchard_root, orchard_size)) = (
                    sapling_root_and_len.unwrap_or_default(),
                    orchard_root_and_len.unwrap_or_default(),
                );

                let data = BlockData {
                    version: block.header.version,
                    time: block.header.time.timestamp(),
                    merkle_root: block.header.merkle_root.0,
                    bits: u32::from_be_bytes(
                        block.header.difficulty_threshold.bytes_in_display_order(),
                    ),
                    block_commitments: match block.commitment(&self.network).map_err(|e| {
                        // Currently, this can only fail when the zebra provides unvalid data.
                        // Sidechain blocks will fail, however, and when we incorporate them
                        // we must adapt this to match
                        SyncError::ZebradConnectionError(NodeConnectionError::UnrecoverableError(
                            Box::new(e),
                        ))
                    })? {
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

                    nonse: *block.header.nonce,
                    solution: block.header.solution.into(),
                };

                let mut transactions = Vec::new();
                for (i, trnsctn) in block.transactions.iter().enumerate() {
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
                        trnsctn.hash().0,
                        transparent,
                        sapling,
                        orchard,
                    );
                    transactions.push(txdata);
                }

                best_tip = (best_tip.0 + 1, block.hash().into());

                let height = Some(best_tip.0);
                let hash = Hash::from(block.hash());
                let parent_hash = Hash::from(block.header.previous_block_hash);
                let chainwork = prev_block.chainwork().add(&ChainWork::from(U256::from(
                    block
                        .header
                        .difficulty_threshold
                        .to_work()
                        .ok_or_else(|| {
                            SyncError::ZebradConnectionError(
                                NodeConnectionError::UnrecoverableError(Box::new(InvalidData(
                                    format!(
                                        "Invalid work field of block {} {:?}",
                                        block.hash(),
                                        height
                                    ),
                                ))),
                            )
                        })?
                        .as_u128(),
                )));

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

                let chainblock = ChainBlock {
                    index,
                    data,
                    transactions,
                    commitment_tree_data,
                };
                new_blocks.push(chainblock.clone());
            } else {
                let mut next_height_down = best_tip.0 - 1;
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
                        .find(|block| block.height() == Some(best_tip.0 - 1))
                        .map(ChainBlock::hash)
                    {
                        Some(hash) => break hash,
                        // There is a hole in our database.
                        // TODO: An error return may be more appropriate here
                        None => next_height_down = next_height_down - 1,
                    }
                };

                best_tip = (next_height_down, *prev_hash);
                // We can't calculate things like chainwork until we
                // know the parent block
                sidechain_blocks.push(block);
            }
        }

        // todo! handle sidechain blocks
        for block in new_blocks {
            if let Err(e) = self.sync_stage_update_loop(block).await {
                return Err(e.into());
            }
        }
        // TODO: connect to finalized state to determine where to truncate
        self.update(if best_tip.0 .0 > 100 {
            best_tip.0 - 100
        } else {
            Height(1)
        })
        .await?;

        Ok(())
    }

    async fn sync_stage_update_loop(&self, block: ChainBlock) -> Result<(), UpdateError> {
        if let Err(e) = self.stage(block.clone()) {
            match *e {
                mpsc::error::TrySendError::Full(_) => {
                    // TODO: connect to finalized state to determine where to truncate
                    self.update(
                        block
                            .index
                            .height
                            // we'll just truncate at the end
                            .unwrap_or(Height(101))
                            - 100,
                    )
                    .await?;
                    Box::pin(self.sync_stage_update_loop(block)).await?;
                }
                mpsc::error::TrySendError::Closed(_block) => {
                    return Err(UpdateError::ReceiverDisconnected)
                }
            }
        }
        Ok(())
    }
    /// Stage a block
    pub fn stage(
        &self,
        block: ChainBlock,
    ) -> Result<(), Box<mpsc::error::TrySendError<ChainBlock>>> {
        self.staging_sender.try_send(block).map_err(Box::new)
    }
    /// Add all blocks from the staging area, and save a new cache snapshot
    pub async fn update(&self, finalized_height: Height) -> Result<(), UpdateError> {
        let mut new = HashMap::<Hash, ChainBlock>::new();
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
        // todo: incorperate with finalized state to synchronize removal of finalized blocks from nonfinalized state instead of discarding below a cetain height
        let (_newly_finalzed, blocks): (HashMap<_, _>, HashMap<Hash, _>) = new
            .into_iter()
            .partition(|(_hash, block)| match block.index().height() {
                Some(height) => height < finalized_height,
                None => false,
            });
        // TODO: At this point, we need to ensure the newly-finalized blocks are known
        // to be in the finalzed state

        let best_tip = blocks.iter().fold(snapshot.best_tip, |acc, (hash, block)| {
            match block.index().height() {
                Some(height) if height > acc.0 => (height, (*hash)),
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
            Ok(())
        } else {
            Err(UpdateError::StaleSnapshot)
        }
    }

    /// Get a copy of the block cache as it existed at the last [`Self::update`] call
    pub fn get_snapshot(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.current.load_full()
    }
}

/// Errors that occur during a snapshot update
pub enum UpdateError {
    /// The block reciever disconnected. This should only happen during shutdown.
    ReceiverDisconnected,
    /// The snapshot was already updated by a different process, between when this update started
    /// and when it completed.
    StaleSnapshot,
}

/// A connection to a validator.
#[derive(Clone, derive_more::From)]
pub enum BlockchainSource {
    /// The connection is via direct read access to a zebrad's data file
    State(ReadStateService),
    /// We are connected to a zebrad, zcashd, or other zainod via JsonRpSee
    Fetch(JsonRpSeeConnector),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BlockchainSourceError {
    // TODO: Add logic for handling recoverable errors if any are identified
    // one candidate may be ephemerable network hiccoughs
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
}

type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

/// Methods that will dispatch to a ReadStateService or JsonRpSeeConnector
impl BlockchainSource {
    pub(super) async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match self {
            BlockchainSource::State(read_state_service) => match read_state_service
                .clone()
                .call(zebra_state::ReadRequest::Block(id))
                .await
            {
                Ok(zebra_state::ReadResponse::Block(block)) => Ok(block),
                Ok(otherwise) => panic!(
                    "Read Request of Block returned Read Response of {otherwise:#?} \n\
                    This should be deterministically unreachable"
                ),
                Err(e) => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
            },
            BlockchainSource::Fetch(json_rp_see_connector) => {
                match json_rp_see_connector
                    .get_block(id.to_string(), Some(0))
                    .await
                {
                    Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                        zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                            .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    ))),
                    Ok(_) => unreachable!(),
                    Err(e) => match e {
                        RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                        RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                        _ => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
                    },
                }
            }
        }
    }

    async fn get_commitment_tree_roots(
        &self,
        id: Hash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        match self {
            BlockchainSource::State(read_state_service) => {
                let (sapling_tree_response, orchard_tree_response) = join(
                    read_state_service
                        .clone()
                        .call(zebra_state::ReadRequest::SaplingTree(HashOrHeight::Hash(
                            id.into(),
                        ))),
                    read_state_service
                        .clone()
                        .call(zebra_state::ReadRequest::OrchardTree(HashOrHeight::Hash(
                            id.into(),
                        ))),
                )
                .await;
                let (sapling_tree, orchard_tree) = match (
                    //TODO: Better readstateservice error handling
                    sapling_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    orchard_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                ) {
                    (ReadResponse::SaplingTree(saptree), ReadResponse::OrchardTree(orctree)) => {
                        (saptree, orctree)
                    }
                    (_, _) => panic!("Bad response"),
                };

                Ok((
                    sapling_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                    orchard_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                ))
            }
            BlockchainSource::Fetch(json_rp_see_connector) => {
                let tree_responses = json_rp_see_connector
                    .get_treestate(id.to_string())
                    .await
                    // As MethodError contains a GetTreestateError, which is an enum with no variants,
                    // we don't need to account for it at all here
                    .map_err(|e| match e {
                        RpcRequestError::ServerWorkQueueFull => {
                            BlockchainSourceError::Unrecoverable(
                                "Not yet implemented: handle backing validator\
                                full queue"
                                    .to_string(),
                            )
                        }
                        _ => BlockchainSourceError::Unrecoverable(e.to_string()),
                    })?;
                let GetTreestateResponse {
                    sapling, orchard, ..
                } = tree_responses;
                let sapling_frontier = sapling
                    .inner()
                    .inner()
                    .as_ref()
                    .map(hex::decode)
                    .transpose()
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not interpret sapling tree of block {id}"))
                                .to_string(),
                        )
                    })?
                    .as_deref()
                    .map(read_commitment_tree::<zebra_chain::sapling::tree::Node, _, 32>)
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let orchard_frontier = orchard
                    .inner()
                    .inner()
                    .as_ref()
                    .map(hex::decode)
                    .transpose()
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not interpret orchard tree of block {id}"))
                                .to_string(),
                        )
                    })?
                    .as_deref()
                    .map(read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>)
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let sapling_root = sapling_frontier
                    .map(|tree| {
                        zebra_chain::sapling::tree::Root::try_from(*tree.root().as_ref())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                let orchard_root = orchard_frontier
                    .map(|tree| {
                        zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                Ok((sapling_root, orchard_root))
            }
        }
    }
}
