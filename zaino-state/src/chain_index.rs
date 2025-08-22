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

use crate::error::{ChainIndexError, ChainIndexErrorKind, FinalisedStateError};
use crate::SyncError;
use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::Stream;
use non_finalised_state::NonfinalizedBlockCacheSnapshot;
use source::{BlockchainSource, ValidatorConnector};
use tokio_stream::StreamExt;
use types::ChainBlock;
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

/// The interface to the chain index
pub trait ChainIndex {
    /// A snapshot of the nonfinalized state, needed for atomic access
    type Snapshot;

    /// How it can fail
    type Error;

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot;

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
    /// given a transaction id, returns the transaction
    fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, Self::Error>>;
    /// Given a transaction ID, returns all known hashes and heights of blocks
    /// containing that transaction. Height is None for blocks not on the best chain.
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<
            std::collections::HashMap<types::BlockHash, Option<types::Height>>,
            Self::Error,
        >,
    >;
}
/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once. Backed by a source of blocks, either
/// a zebra ReadStateService (direct read access to a running
/// zebrad's database) or a jsonRPC connection to a validator.
///
/// Currently does not support mempool operations
pub struct NodeBackedChainIndex<Source: BlockchainSource = ValidatorConnector> {
    // TODO: mempool
    non_finalized_state: std::sync::Arc<crate::NonFinalizedState<Source>>,
    // pub crate required for unit tests, this can be removed once we implement finalised state sync.
    pub(crate) finalized_db: std::sync::Arc<finalised_state::ZainoDB>,
    finalized_state: finalised_state::reader::DbReader,
}

impl<Source: BlockchainSource> NodeBackedChainIndex<Source> {
    /// Creates a new chainindex from a connection to a validator
    /// Currently this is a ReadStateService or JsonRpSeeConnector
    pub async fn new(
        source: Source,
        config: crate::config::BlockCacheConfig,
    ) -> Result<Self, crate::InitError>
where {
        use futures::TryFutureExt as _;

        let (non_finalized_state, finalized_db) = futures::try_join!(
            crate::NonFinalizedState::initialize(source.clone(), config.network.clone(), None),
            finalised_state::ZainoDB::spawn(config, source)
                .map_err(crate::InitError::FinalisedStateInitialzationError)
        )?;
        let finalized_db = std::sync::Arc::new(finalized_db);
        let chain_index = Self {
            non_finalized_state: std::sync::Arc::new(non_finalized_state),
            finalized_state: finalized_db.to_reader(),
            finalized_db,
        };
        chain_index.start_sync_loop();
        Ok(chain_index)
    }
}

impl<Source: BlockchainSource> NodeBackedChainIndex<Source> {
    pub(super) fn start_sync_loop(
        &self,
    ) -> tokio::task::JoinHandle<Result<std::convert::Infallible, SyncError>> {
        let nfs = self.non_finalized_state.clone();
        let fs = self.finalized_db.clone();
        tokio::task::spawn(async move {
            loop {
                nfs.sync(fs.clone()).await?;
                //TODO: configure sleep duration?
                //TODO: sync finalized state
                tokio::time::sleep(Duration::from_millis(500)).await
            }
        })
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
    ) -> Result<impl Iterator<Item = ChainBlock> + use<'iter, Source>, FinalisedStateError>
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
                //TODO: chain with mempool when available
            ))
    }
}

impl<Source: BlockchainSource> ChainIndex for NodeBackedChainIndex<Source> {
    type Snapshot = Arc<NonfinalizedBlockCacheSnapshot>;
    type Error = ChainIndexError;

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot {
        self.non_finalized_state.get_snapshot()
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
        let end = end.unwrap_or(nonfinalized_snapshot.best_tip.0);
        if end <= nonfinalized_snapshot.best_tip.0 {
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

    /// given a transaction id, returns the transaction
    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        // TODO: mempool?
        let Some(block) = self
            .blocks_containing_transaction(snapshot, txid.0)
            .await?
            .next()
        else {
            return Ok(None);
        };

        let full_block = self
            .non_finalized_state
            .source
            .get_block(HashOrHeight::Hash((block.index().hash().0).into()))
            .await
            .map_err(ChainIndexError::backing_validator)?
            .ok_or_else(|| ChainIndexError::database_hole(block.index().hash()))?;
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
            .map(Some)
    }

    /// Given a transaction ID, returns all known blocks containing this transaction
    /// At most one of these blocks will be on the best chain
    ///
    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<HashMap<types::BlockHash, std::option::Option<types::Height>>, ChainIndexError>
    {
        // TODO: mempool
        Ok(self
            .blocks_containing_transaction(snapshot, txid.0)
            .await?
            .map(|block| (*block.hash(), block.height()))
            .collect())
    }
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&ChainBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&ChainBlock>;
}

impl NonFinalizedSnapshot for NonfinalizedBlockCacheSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&ChainBlock> {
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
