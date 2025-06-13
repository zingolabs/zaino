use std::{collections::HashMap, mem, sync::Arc};

use crate::chain_index::types::{Hash, Height};
use arc_swap::ArcSwap;
use tokio::sync::{mpsc, RwLock};
use tower::Service;
use zaino_fetch::jsonrpsee::{connector::JsonRpSeeConnector, response::GetBlockResponse};
use zebra_chain::serialization::ZcashDeserialize;
use zebra_state::{HashOrHeight, ReadStateService};

use crate::ChainBlock;

/// Holds the block cache
struct NonFinalzedState {
    /// We need access to the validator's best block hash, as well
    /// as a source of blocks
    source: BlockchainSource,
    staged: RwLock<mpsc::UnboundedReceiver<ChainBlock>>,
    staging_sender: mpsc::UnboundedSender<ChainBlock>,
    /// This lock should not be exposed to consumers. Rather,
    /// clone the Arc and offer that. This means we can overwrite the arc
    /// without interfering with readers, who will hold a stale copy
    current: ArcSwap<NonfinalizedBlockCacheSnapshot>,
}

pub(crate) struct NonfinalizedBlockCacheSnapshot {
    /// the set of all known blocks < 100 blocks old
    /// this includes all blocks on-chain, as well as
    /// all blocks known to have been on-chain before being
    /// removed by a reorg. Blocks reorged away have no height.
    blocks: HashMap<Hash, ChainBlock>,
    // Do we need height here?
    /// The highest known block
    best_tip: (Height, Hash),
}

/// This is the core of the concurrent block cache.
impl NonFinalzedState {
    /// sync to the top of the chain
    ///
    /// TODO: This function currently only handles sync from the case where a
    /// NonFinalizedState already exists, and is not yet optimized for the case
    /// where more than 100 blocks have been mined since the last sync call
    pub async fn sync(&self) {
        let best_hash = match self.source.get_tip().await {
            Ok(Some((_height, hash))) => hash,
            Ok(None) => todo!(),
            Err(_) => todo!(),
        };

        let initial_state = self.get_snapshot().await;
        let mut new_blocks = Vec::new();
        let mut best_tip = initial_state.best_tip.clone();
        loop {
            let next_block = match self
                .source
                .get_block(HashOrHeight::Height(zebra_chain::block::Height(
                    u32::from(best_tip.0) + 1,
                )))
                .await
            {
                Ok(block) => block,
                Err(_) => todo!(),
            };

            match next_block {
                Some(block) => {
                    // If this block is next in the chain, we sync it as normal
                    if Hash::from(block.header.previous_block_hash) == initial_state.best_tip.1 {
                        best_tip = (best_tip.0 + 1, block.hash().into());
                        new_blocks.push(block);
                    } else {
                        // If not, there's been a reorg, and we need to adjust our best-tip
                        let prev_hash = initial_state
                            .blocks
                            .values()
                            .find(|block| block.height() == Some(best_tip.0 - 1))
                            .map(ChainBlock::hash)
                            .unwrap_or_else(|| todo!("handle holes in database?"));

                        best_tip = (best_tip.0 - 1, *prev_hash);
                        new_blocks.push(block);
                    }
                }
                // This should probably be concurrent rather than waiting until
                // all blocks are received
                // TODO: send all blocks to the sender, and update
                None => todo!(),
            }
        }
    }
    /// Stage a block
    pub fn stage(&self, block: ChainBlock) -> Result<(), mpsc::error::SendError<ChainBlock>> {
        self.staging_sender.send(block)
    }
    /// Add all blocks from the staging area, and save a new cache snapshot
    pub async fn update(&self, finalized_height: Height) -> Result<(), ()> {
        let mut new = HashMap::<Hash, ChainBlock>::new();
        let mut staged = self.staged.write().await;
        loop {
            match staged.try_recv() {
                Ok(chain_block) => {
                    new.insert(*chain_block.index().hash(), chain_block);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Err(()),
            }
        }
        // at this point, we've collected everything in the staging area
        // we can drop the stage lock, and more blocks can be staged while we finish setting current
        mem::drop(staged);
        let snapshot = self.get_snapshot().await;
        new.extend(
            snapshot
                .blocks
                .iter()
                .map(|(hash, block)| (hash.clone(), block.clone())),
        );
        let (newly_finalzed, blocks): (HashMap<_, _>, HashMap<Hash, _>) = new
            .into_iter()
            .partition(|(_hash, block)| match block.index().height() {
                Some(height) => height <= finalized_height,
                None => false,
            });
        // TODO: At this point, we need to ensure the newly-finalized blocks are known
        // to be in the finalzed state

        let best_tip = blocks.iter().fold(snapshot.best_tip, |acc, (hash, block)| {
            match block.index().height() {
                Some(height) if height > acc.0 => (height, (*hash).clone()),
                _ => acc,
            }
        });
        // Need to get best hash at some point in this process
        self.current.store(Arc::new(NonfinalizedBlockCacheSnapshot {
            blocks,
            best_tip,
        }));

        Ok(())
    }

    /// Get a copy of the block cache as it existed at the last [update] call
    pub async fn get_snapshot(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.current.load_full()
    }
}

/// A connection to a validator.
#[derive(Clone)]
enum BlockchainSource {
    State(ReadStateService),
    Fetch(JsonRpSeeConnector),
}

struct BlockchainSourceError {}

type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

impl BlockchainSource {
    async fn get_tip(
        &self,
    ) -> Result<Option<(zebra_chain::block::Height, zebra_chain::block::Hash)>, BlockchainSourceError>
    {
        match self {
            BlockchainSource::State(read_state_service) => {
                let response = match read_state_service
                    .clone()
                    .call(zebra_state::ReadRequest::Tip)
                    .await
                {
                    Ok(resp) => resp,
                    Err(_) => todo!(),
                };
                match response {
                    zebra_state::ReadResponse::Tip(tip) => Ok(tip),
                    _ => unreachable!("bad read response"),
                }
            }
            BlockchainSource::Fetch(json_rp_see_connector) => {
                match json_rp_see_connector.get_blockchain_info().await {
                    Ok(info) => Ok(Some((info.blocks, info.best_block_hash))),
                    Err(_) => todo!(),
                }
            }
        }
    }

    async fn get_block(
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
                Ok(_) => todo!(),
                Err(_) => todo!(),
            },
            BlockchainSource::Fetch(json_rp_see_connector) => {
                match json_rp_see_connector
                    .get_block(id.to_string(), Some(0))
                    .await
                {
                    Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                        zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                            .unwrap_or_else(|_e| todo!("block too large")),
                    ))),
                    Ok(_) => unreachable!(),
                    Err(_) => todo!(),
                }
            }
        }
    }
}
