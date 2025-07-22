use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    future::Future,
    sync::Arc,
};

use super::non_finalised_state::{
    BlockchainSource, InitError, NonFinalizedState, NonfinalizedBlockCacheSnapshot,
};
use super::types::{self, ChainBlock};
use futures::Stream;
use tokio_stream::StreamExt;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_chain::serialization::ZcashSerialize;
use zebra_state::HashOrHeight;

/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once. Backed by a source of blocks, either
/// a zebra ReadStateService (direct read access to a running
/// zebrad's database) or a jsonRPC connection to a validator.
///
/// TODO: Currently only contains the non-finalized state.
pub struct NodeBackedChainIndex {
    // TODO: finalized state
    // TODO: mempool
    non_finalized_state: NonFinalizedState,
}

impl NodeBackedChainIndex {
    /// Creates a new chainindex from a connection to a validator
    /// Currently this is a ReadStateService or JsonRpSeeConnector
    pub async fn new<T: Into<BlockchainSource>>(
        source: T,
        network: ZebraNetwork,
    ) -> Result<Self, InitError> {
        Ok(Self {
            non_finalized_state: NonFinalizedState::initialize(source.into(), network).await?,
        })
    }
    async fn get_fullblock_bytes_from_node(
        &self,
        id: HashOrHeight,
    ) -> Result<Option<Vec<u8>>, GetFullBlockError> {
        match self.non_finalized_state.source.get_block(id).await {
            Ok(block) => block
                .map(|bk| {
                    bk.zcash_serialize_to_vec()
                        .map_err(|e| GetFullBlockError::BackingNodeFailure(Box::new(e)))
                })
                .transpose(),
            Err(e) => Err(GetFullBlockError::BackingNodeFailure(Box::new(e))),
        }
    }
    fn get_chainblock_by_hashorheight<'snapshot: 'future, 'self_lt: 'future, 'future>(
        &'self_lt self,
        non_finalized_snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        height: &HashOrHeight,
    ) -> Option<&'future ChainBlock> {
        //TODO: finalized state
        non_finalized_snapshot.get_chainblock_by_hashorheight(height)
    }

    fn blocks_containing_transaction<'snapshot: 'iter, 'self_lt: 'iter, 'iter>(
        &'self_lt self,
        snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> impl Iterator<Item = &'iter ChainBlock> {
        //TODO: finalized state, mempool
        snapshot.blocks.values().filter_map(move |block| {
            block.transactions().iter().find_map(|transaction| {
                if *transaction.txid() == txid {
                    Some(block)
                } else {
                    None
                }
            })
        })
    }
}

/// The interface to the chain index
pub trait ChainIndexInterface: Sized {
    /// A snapshot of the nonfinalized state, needed for atomic access
    type Snapshot: NonFinalizedSnapshot;
    #[allow(missing_docs)]
    type FindForkPointError;
    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Arc<Self::Snapshot>;

    /// Given inclusive start and end indexes, stream all blocks
    /// between the given indexes. Can be specified
    /// by hash or height.
    fn get_block_range<'snapshot, 'self_lt, 'future>(
        &'self_lt self,
        nonfinalized_snapshot: &'snapshot Self::Snapshot,
        start: Option<HashOrHeight>,
        end: Option<HashOrHeight>,
    ) -> Result<impl Stream<Item = Result<Vec<u8>, GetBlockRangeError>> + 'future, GetBlockRangeError>
    where
        'snapshot: 'future,
        'self_lt: 'future;
    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: impl AsRef<Self::Snapshot>,
        block_hash: &types::Hash,
    ) -> Result<Option<(types::Hash, types::Height)>, Self::FindForkPointError>;
    /// given a transaction id, returns the transaction
    fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: [u8; 32],
    ) -> impl Future<Output = Result<Option<Vec<u8>>, ()>>;
    /// Given a transaction ID, returns all known
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: [u8; 32],
    ) -> HashMap<types::Hash, Option<types::Height>>;
}

impl ChainIndexInterface for NodeBackedChainIndex {
    type Snapshot = NonfinalizedBlockCacheSnapshot;
    type FindForkPointError = std::convert::Infallible;

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Arc<Self::Snapshot> {
        self.non_finalized_state.get_snapshot()
    }

    /// Given inclusive start and end indexes, stream all blocks
    /// between the given indexes. Can be specified
    /// by hash or height.
    fn get_block_range<'snapshot: 'future, 'self_lt: 'future, 'future>(
        &'self_lt self,
        nonfinalized_snapshot: &'snapshot Self::Snapshot,
        start: Option<HashOrHeight>,
        end: Option<HashOrHeight>,
    ) -> Result<impl Stream<Item = Result<Vec<u8>, GetBlockRangeError>> + 'future, GetBlockRangeError>
    {
        // with no start supplied, start from genesis
        let Some(start_block) = self.get_chainblock_by_hashorheight(
            nonfinalized_snapshot,
            &start.unwrap_or(HashOrHeight::Height(zebra_chain::block::Height(1))),
        ) else {
            return Err(GetBlockRangeError {
                kind: GetBlockRangeErrorKind::MissingStartBlock,
                details: None,
            });
        };
        let Some(end_block) = self.get_chainblock_by_hashorheight(
            nonfinalized_snapshot,
            &end.unwrap_or(HashOrHeight::Height(zebra_chain::block::Height(1))),
        ) else {
            return Err(GetBlockRangeError {
                kind: GetBlockRangeErrorKind::MissingEndBlock,
                details: None,
            });
        };

        let mut nonfinalized_block = nonfinalized_snapshot.get_chainblock_by_hash(end_block.hash());
        let first_nonfinalized_hash = nonfinalized_snapshot
            .get_chainblock_by_hash(start_block.hash())
            .map(|block| block.index().hash());

        // TODO: combine with finalized state when available
        let mut nonfinalized_range = vec![];
        while let Some(block) = nonfinalized_block {
            nonfinalized_range.push(*block.hash());
            nonfinalized_block = if Some(block.index().parent_hash()) != first_nonfinalized_hash {
                nonfinalized_snapshot.get_chainblock_by_hash(block.index().parent_hash())
            } else {
                None
            }
        }

        Ok(tokio_stream::iter(nonfinalized_range).then(async |hash| {
            self.get_fullblock_bytes_from_node(HashOrHeight::Hash(hash.into()))
                .await
                .map_err(|e| GetBlockRangeError {
                    kind: GetBlockRangeErrorKind::BackingNodeFailure,
                    details: Some(e.to_string()),
                })?
                .ok_or(GetBlockRangeError {
                    kind: GetBlockRangeErrorKind::BackingNodeFailure,
                    details: Some(format!("hole in validator database, missing block {hash}")),
                })
        }))
    }

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: impl AsRef<Self::Snapshot>,
        block_hash: &types::Hash,
    ) -> Result<Option<(types::Hash, types::Height)>, Self::FindForkPointError> {
        let Some(block) = snapshot.as_ref().get_chainblock_by_hash(block_hash) else {
            // No fork point found. This is not an error,
            // as zaino does not guarentee knowledge of all sidechain data.
            return Ok(None);
        };
        if let Some(height) = block.height() {
            Ok(Some((*block.hash(), height)))
        } else {
            self.find_fork_point(&snapshot, block.index().parent_hash())
        }
    }

    /// given a transaction id, returns the transaction
    async fn get_raw_transaction(
        &self,
        snapshot: &NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> Result<Option<Vec<u8>>, ()> {
        let Some(block) = self.blocks_containing_transaction(snapshot, txid).next() else {
            return Ok(None);
        };
        let full_block = self
            .non_finalized_state
            .source
            .get_block(HashOrHeight::Hash((*block.index().hash()).into()))
            .await
            //TODO: error handle
            .map_err(|_| ())?
            .ok_or_else::<(), _>(|| todo!("hole in zebra database"))?;
        full_block
            .transactions
            .iter()
            .find(|transaction| transaction.hash().0 == txid)
            .map(ZcashSerialize::zcash_serialize_to_vec)
            .ok_or_else::<(), _>(|| todo!("hole in zebra database"))?
            .map_err(|_e| todo!("write to vec failed???"))
            .map(Some)
    }

    /// Given a transaction ID, returns all known blocks containing this transaction
    /// At most one of these blocks will be on the best chain
    ///
    fn get_transaction_status(
        &self,
        snapshot: &NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> HashMap<types::Hash, Option<types::Height>> {
        self.blocks_containing_transaction(snapshot, txid)
            .map(|block| (*block.hash(), block.height()))
            .collect()
    }
}

/// Fork point errors
pub enum FindForkPointError {
    /// A block in the fork chain could not be found in the non-finalized state
    /// NOTE: Non-best chains are not currently stored in the finalized state. If the fork point
    /// is in the finalized state, this will cause a MissingBlock error
    MissingBlock,
}

/// The full error
pub struct GetBlockRangeError {
    /// What went wrong
    pub kind: GetBlockRangeErrorKind,
    /// How it went wrong
    pub details: Option<String>,
}

/// The things that can go wrong getting a block range
pub enum GetBlockRangeErrorKind {
    /// The block at the provided start index could not be found. Likely, an incorrect hash
    /// was supplied
    MissingStartBlock,
    /// The block at the provided start end could not be found. Likely, an incorrect hash
    /// was supplied
    MissingEndBlock,
    /// The query to the validator failed. This is likely unrecoverable,
    /// TODO make sure to separate transitive network errors
    BackingNodeFailure,
}

#[derive(thiserror::Error, Debug)]
enum GetFullBlockError {
    #[error("0")]
    BackingNodeFailure(Box<dyn Disbug + Send + Sync + 'static>),
}

trait Disbug: Display + Debug {}
impl<T: Display + Debug> Disbug for T {}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Convenience fn
    fn get_chainblock_by_hashorheight(&self, target: &HashOrHeight) -> Option<&ChainBlock> {
        match target {
            HashOrHeight::Hash(hash) => self.get_chainblock_by_hash(&types::Hash::from(*hash)),
            HashOrHeight::Height(height) => self.get_chainblock_by_height(&types::Height(height.0)),
        }
    }
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::Hash) -> Option<&ChainBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&ChainBlock>;
}

impl NonFinalizedSnapshot for NonfinalizedBlockCacheSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::Hash) -> Option<&ChainBlock> {
        self.blocks.iter().find_map(|(hash, chainblock)| {
            if hash == target_hash {
                Some(chainblock)
            } else {
                None
            }
        })
    }
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&ChainBlock> {
        self.heights_to_hashes.iter().find_map(|(height, hash)| {
            if height == target_height {
                self.get_chainblock_by_hash(hash)
            } else {
                None
            }
        })
    }
}
