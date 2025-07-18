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

pub mod encoding;
/// All state at least 100 blocks old
pub mod finalised_state;
/// State in the mempool, not yet on-chain
pub mod mempool;
/// State less than 100 blocks old, stored separately as it may be reorged
pub mod non_finalised_state;
/// Common types used by the rest of this module
pub mod types;

use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::{Debug, Display},
    sync::Arc,
};

use futures::Stream;
use non_finalised_state::{BlockchainSource, NonFinalizedState, NonfinalizedBlockCacheSnapshot};
use tokio_stream::StreamExt;
use types::ChainBlock;
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
pub struct ChainIndex {
    // TODO: finalized state
    // TODO: mempool
    non_finalized_state: non_finalised_state::NonFinalizedState,
}

impl ChainIndex {
    /// Creates a new chainindex from a connection to a validator
    /// Currently this is a ReadStateService or JsonRpSeeConnector
    pub async fn new<T: Into<BlockchainSource>>(
        source: T,
        network: ZebraNetwork,
    ) -> Result<Self, non_finalised_state::InitError> {
        Ok(Self {
            non_finalized_state: NonFinalizedState::initialize(source.into(), network).await?,
        })
    }

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    pub fn snapshot_nonfinalized_state(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.non_finalized_state.get_snapshot()
    }

    /// Given inclusive start and end indexes, stream all blocks
    /// between the given indexes. Can be specified
    /// by hash or height.
    pub fn get_block_range<'snapshot: 'future, 'self_lt: 'future, 'future>(
        &'self_lt self,
        nonfinalized_snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot> + Clone + 'snapshot,
        start: Option<HashOrHeight>,
        end: Option<HashOrHeight>,
    ) -> Result<impl Stream<Item = Result<Vec<u8>, GetBlockRangeError>> + 'future, GetBlockRangeError>
    {
        // with no start supplied, start from genesis
        let Some(start_block) = self.get_chainblock_by_hashorheight(
            nonfinalized_snapshot.as_ref(),
            &start.unwrap_or(HashOrHeight::Height(zebra_chain::block::Height(1))),
        ) else {
            return Err(GetBlockRangeError {
                kind: GetBlockRangeErrorKind::MissingStartBlock,
                details: None,
            });
        };
        let Some(end_block) = self.get_chainblock_by_hashorheight(
            nonfinalized_snapshot.as_ref(),
            &end.unwrap_or(HashOrHeight::Height(zebra_chain::block::Height(1))),
        ) else {
            return Err(GetBlockRangeError {
                kind: GetBlockRangeErrorKind::MissingEndBlock,
                details: None,
            });
        };

        let mut nonfinalized_block = nonfinalized_snapshot
            .as_ref()
            .get_chainblock_by_hash(end_block.hash());
        let first_nonfinalized_hash = nonfinalized_snapshot
            .as_ref()
            .get_chainblock_by_hash(start_block.hash())
            .map(|block| block.index().hash());

        // TODO: combine with finalized state when available
        let mut nonfinalized_range = vec![];
        while let Some(block) = nonfinalized_block {
            nonfinalized_range.push(block.hash().clone());
            nonfinalized_block = if Some(block.index().parent_hash()) != first_nonfinalized_hash {
                nonfinalized_snapshot
                    .as_ref()
                    .get_chainblock_by_hash(block.index().parent_hash())
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

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    pub fn find_fork_point(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        block_hash: &types::Hash,
    ) -> Result<(types::Hash, types::Height), FindForkPointError> {
        let block = snapshot
            .as_ref()
            .get_chainblock_by_hash(&block_hash)
            .ok_or(FindForkPointError::MissingBlock)?;
        if let Some(height) = block.height() {
            return Ok((*block.hash(), height));
        } else {
            self.find_fork_point(&snapshot, block.index().parent_hash())
        }
    }

    /// given a transaction id, returns the transaction
    pub async fn get_raw_transaction(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        txid: zebra_chain::transaction::Hash,
    ) -> Result<Option<Vec<u8>>, ()> {
        let Some((block, txindex)) = snapshot.as_ref().blocks.values().find_map(|block| {
            block.transactions().iter().find_map(|transaction| {
                if *transaction.txid() == txid.0 {
                    Some((block, transaction.index()))
                } else {
                    None
                }
            })
        }) else {
            return Ok(None);
        };
        let full_block = self
            .non_finalized_state
            .source
            .get_block(HashOrHeight::Hash((*block.index().hash()).into()))
            .await
            //TODO: error handle
            .map_err(|_| ())?
            .ok_or_else(|| todo!("hole in zebra database"))?;
        full_block
            .transactions
            .iter()
            .find(|transaction| transaction.hash() == txid)
            .map(ZcashSerialize::zcash_serialize_to_vec)
            .ok_or_else(|| todo!("hole in zebra database"))?
            .map_err(|e| todo!("write to vec failed???"))
            .map(Some)
    }

    /// Given a transaction ID, returns all known
    pub async fn get_transaction_status(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        txid: zebra_chain::transaction::Hash,
    ) -> HashMap<zebra_chain::block::Hash, Option<zebra_chain::block::Height>> {
        todo!()
    }

    fn get_chainblock_by_hashorheight<'snapshot: 'future, 'self_lt: 'future, 'future>(
        &'self_lt self,
        non_finalized_snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        height: &HashOrHeight,
    ) -> Option<&'future ChainBlock> {
        //TODO: finalized state
        non_finalized_snapshot.get_chainblock_by_hashorheight(height)
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
