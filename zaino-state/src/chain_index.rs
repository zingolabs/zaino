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
    ///
    /// Also returns a bool representing whether the transaction is *currently* in the mempool.
    /// This is not currently tied to the given snapshot but rather uses the live mempool.
    #[allow(clippy::type_complexity)]
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<
            (
                std::collections::HashMap<types::BlockHash, Option<types::Height>>,
                bool,
            ),
            Self::Error,
        >,
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
    /// If the chain tip has changed from the given spanshot an IncorrectChainTip error is returned.
    /// NOTE: Is this correct here?
    #[allow(clippy::type_complexity)]
    fn get_mempool_stream(
        &self,
        snapshot: &Self::Snapshot,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;
}

/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once. Backed by a source of blocks, either
/// a zebra ReadStateService (direct read access to a running
/// zebrad's database) or a jsonRPC connection to a validator.
pub struct NodeBackedChainIndex<Source: BlockchainSource = ValidatorConnector> {
    _mempool_state: std::sync::Arc<mempool::Mempool<Source>>,
    mempool: mempool::MempoolSubscriber,
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

        let (mempool_state, non_finalized_state, finalized_db) = futures::try_join!(
            mempool::Mempool::spawn(source.clone(), None)
                .map_err(crate::InitError::MempoolInitialzationError),
            crate::NonFinalizedState::initialize(source.clone(), config.network.clone(), None),
            finalised_state::ZainoDB::spawn(config, source)
                .map_err(crate::InitError::FinalisedStateInitialzationError)
        )?;
        let finalized_db = std::sync::Arc::new(finalized_db);
        let chain_index = Self {
            mempool: mempool_state.subscriber(),
            _mempool_state: std::sync::Arc::new(mempool_state),
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
                {
                    let snapshot = nfs.get_snapshot();
                    while snapshot.best_tip.0 .0
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
                        fs.write_block(next_finalized_block.clone())
                            .await
                            .map_err(|_e| SyncError::CompetingSyncProcess)?;
                    }
                }
                //TODO: configure sleep duration?
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
        if let Some(mempool_tx) = self
            .mempool
            .get_transaction(&mempool::MempoolKey(txid.to_string()))
            .await
        {
            let bytes = mempool_tx.0.as_ref().as_ref().to_vec();
            return Ok(Some(bytes));
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
    /// Also returns a bool representing whether the transaction is *currently* in the mempool.
    /// This is not currently tied to the given snapshot but rather uses the live mempool.
    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<
        (
            HashMap<types::BlockHash, std::option::Option<types::Height>>,
            bool,
        ),
        ChainIndexError,
    > {
        Ok((
            self.blocks_containing_transaction(snapshot, txid.0)
                .await?
                .map(|block| (*block.hash(), block.height()))
                .collect(),
            self.mempool
                .contains_txid(&mempool::MempoolKey(txid.to_string()))
                .await,
        ))
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
            .map(|(_, v)| v.0.as_ref().as_ref().to_vec())
            .collect();

        Ok(bytes)
    }

    /// Returns a stream of mempool transactions, ending the stream when the chain tip block hash
    /// changes (a new block is mined or a reorg occurs).
    ///
    /// If the chain tip has changed from the given snapshot at the time of calling
    /// an IncorrectChainTip error is returned holding the current chain tip block hash.
    /// NOTE: Is this correct here?
    fn get_mempool_stream(
        &self,
        snapshot: &Self::Snapshot,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let expected_chain_tip = snapshot.best_tip.1;

        let mut subscriber = self.mempool.clone();

        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, ChainIndexError>>(32);

        tokio::spawn(async move {
            match subscriber
                .get_mempool_stream(Some(expected_chain_tip))
                .await
            {
                Ok((in_rx, _handle)) => {
                    let mut in_stream = tokio_stream::wrappers::ReceiverStream::new(in_rx);
                    while let Some(item) = in_stream.next().await {
                        match item {
                            Ok((_key, value)) => {
                                let _ = out_tx.send(Ok(value.0.as_ref().as_ref().to_vec())).await;
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
                }
                Err(e) => {
                    let _ = out_tx.send(Err(e.into())).await;
                }
            }
        });

        Some(tokio_stream::wrappers::ReceiverStream::new(out_rx))
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
