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

use std::{collections::HashMap, sync::Arc};

use futures::{stream, Stream};
use non_finalised_state::{BlockchainSource, NonFinalizedState, NonfinalizedBlockCacheSnapshot};
use types::ChainBlock;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_state::{HashOrHeight, ReadStateService};

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

    /// Takes a snapshot of the non_finalized state. All query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    pub fn snapshot_nonfinalized_state(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.non_finalized_state.get_snapshot()
    }

    /// Given inclusive start and end indexes, stream all blocks
    /// between the given indexes. Can be specified
    /// by hash or height.
    pub fn get_block_range(
        &self,
        nonfinalized_snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot> + Clone,
        start: Option<HashOrHeight>,
        end: Option<HashOrHeight>,
    ) -> Result<impl Stream<Item = Box<[u8]>>, GetBlockRangeError> {
        let Some(start_block) = (match start {
            Some(HashOrHeight::Hash(hash)) => {
                self.get_block_by_hash(nonfinalized_snapshot.clone(), &hash.into())
            }
            Some(HashOrHeight::Height(height)) => {
                self.get_block_by_height(nonfinalized_snapshot.clone(), types::Height(height.0))
            }
            // start from the beginning
            None => self.get_block_by_height(nonfinalized_snapshot.clone(), types::Height(1)),
        }) else {
            return Err(GetBlockRangeError::MissingStartBlock);
        };
        let Some(end_block) = (match end {
            Some(HashOrHeight::Hash(hash)) => {
                self.get_block_by_hash(nonfinalized_snapshot.clone(), &hash.into())
            }
            Some(HashOrHeight::Height(height)) => {
                self.get_block_by_height(nonfinalized_snapshot.clone(), types::Height(height.0))
            }
            //
            None => self.get_block_by_height(
                nonfinalized_snapshot.clone(),
                nonfinalized_snapshot.as_ref().best_tip.0,
            ),
        }) else {
            return Err(GetBlockRangeError::MissingEndBlock);
        };

        let mut nonfinalized_block = nonfinalized_snapshot
            .as_ref()
            .get_block_by_hash(end_block.hash());
        let first_nonfinalized_hash = nonfinalized_snapshot
            .as_ref()
            .get_block_by_hash(start_block.hash())
            .map(|block| block.index().hash());

        let mut nonfinalized_range = vec![];
        while let Some(block) = nonfinalized_block {
            nonfinalized_range.push(block.hash().clone());
            nonfinalized_block = if Some(block.index().parent_hash()) != first_nonfinalized_hash {
                nonfinalized_snapshot
                    .as_ref()
                    .get_block_by_hash(block.index().parent_hash())
            } else {
                None
            }
        }
        // At this point, nonfinalized_range should contain all of the requested
        // range's blocks in reverse order. One the finalized state has finished streaming, these
        // will be streamed from the top of the vec down.

        Ok(tokio_stream::iter(vec![todo!()]))
    }

    fn get_block_by_hash(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        block_hash: &types::Hash,
    ) -> Option<ChainBlock> {
        todo!()
    }

    fn get_block_by_height(
        &self,
        nonfinalized_snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        height: types::Height,
    ) -> Option<ChainBlock> {
        todo!()
    }

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    /// Returns None if there is no common ancestor
    pub async fn find_fork_point(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        block_hash: zebra_chain::block::Hash,
    ) -> Option<(zebra_chain::block::Hash, zebra_chain::block::Height)> {
        todo!()
    }

    /// given a transaction id, returns the transaction
    pub async fn get_raw_transaction(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        txid: zebra_chain::transaction::Hash,
    ) -> Option<Box<[u8]>> {
        todo!()
    }

    /// Given a transaction ID, returns all known
    pub async fn get_transaction_status(
        &self,
        snapshot: impl AsRef<NonfinalizedBlockCacheSnapshot>,
        txid: zebra_chain::transaction::Hash,
    ) -> HashMap<zebra_chain::block::Hash, Option<zebra_chain::block::Height>> {
        todo!()
    }
}

pub enum GetBlockRangeError {
    MissingStartBlock,
    MissingEndBlock,
}
