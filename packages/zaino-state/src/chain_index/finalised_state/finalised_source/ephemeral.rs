//! EphemeralFinalisedState provides access to the finalised portion of the
//! chain when the FinalisedState is syncing, migrating, or switched off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use primitive_types::U256;
use tokio::sync::Mutex;
use tonic::async_trait;
use zaino_common::status::StatusType;
use zaino_proto::proto::compact_formats::CompactBlock;
use zcash_protocol::consensus::Parameters as _;
use zebra_state::HashOrHeight;

use crate::chain_index::finalised_state::capability::{DbCore, DbWrite};
use crate::chain_index::finalised_state::DbMetadata;
use crate::chain_index::source::BlockchainSourceError;
use crate::chain_index::{
    finalised_state::capability::{
        BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbRead,
        IndexedBlockExt,
    },
    source::{BlockchainSource, GetTransactionLocation},
};
use crate::{
    error::FinalisedStateError, BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream,
    Height, IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx,
    SaplingTxList, TransactionHash, TransparentCompactTx, TransparentTxList, TxLocation,
    TxOutCompact, TxidList,
};
use crate::{BlockContext, BlockMetadata, BlockWithMetadata, ChainWork, NamedAtomicStatus};

use zaino_proto::proto::utils::{compact_block_with_pool_types, PoolTypeFilter};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::types::AddrEventBytes, AddrScript};

const EPHEMERAL_FINALISED_STATE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Source-backed finalised-state backend used when persistent finalised-state storage is not
/// serving normal requests.
///
/// `EphemeralFinalisedState` does not own or mutate an on-disk database. Instead, it answers
/// finalised-state read requests by querying the backing [`BlockchainSource`] directly and building
/// the database-facing response types on demand.
///
/// This backend has two intended roles:
///
/// - In ephemeral mode, it is the real finalised-state backend. No persistent database exists, so
///   [`DbRead::db_height`] reports zero via `db_height == None`.
/// - During sync or migration, it is a temporary service-routing backend. Reads are served from the
///   backing source while the persistent database is being written, rebuilt, or migrated elsewhere.
///   In this mode, `db_height` tracks the actual persistent database height so routed callers still
///   observe progress relative to the on-disk database rather than the source tip.
///
/// The struct is cloneable because several async tasks and streaming calls may need handles to the
/// same source-backed backend. Shared runtime state is stored behind [`Arc`] so clones observe the
/// same status, shutdown signal, status-poll task handle, and reported persistent database height.
#[derive(Debug, Clone)]
pub(crate) struct EphemeralFinalisedState<T: BlockchainSource> {
    /// Backing blockchain source used to answer finalised-state reads.
    ///
    /// This is typically a validator/source service. Ephemeral read methods fetch blocks,
    /// transactions, commitment tree data, and chain metadata from this source and convert them into
    /// the same response types exposed by persistent database backends.
    source: T,

    /// Network whose consensus rules are used when reconstructing finalised-state data.
    ///
    /// This is required for network-upgrade checks, especially when deciding whether Sapling or
    /// Orchard commitment tree data is expected for a block.
    network: zebra_chain::parameters::Network,

    /// Current runtime status of the ephemeral backend.
    ///
    /// The background status-poll task updates this value by periodically checking whether the
    /// backing [`BlockchainSource`] is reachable. [`DbCore::status`] returns this value directly.
    status: NamedAtomicStatus,

    /// Shared shutdown signal for the ephemeral backend.
    ///
    /// This flag is set by [`DbCore::shutdown`] and by [`Drop`]. The background status-poll task
    /// observes it and exits when shutdown has been requested.
    shutdown_requested: Arc<AtomicBool>,

    /// Handle for the background status-poll task.
    ///
    /// The task periodically probes the backing source and updates [`Self::status`]. The handle is
    /// stored behind a Tokio mutex so async shutdown can take and await or abort the task exactly
    /// once, even when multiple clones of the ephemeral backend exist.
    status_poll_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,

    /// Reported height of the persistent on-disk database.
    ///
    /// This value is deliberately independent of the backing source height. The source may be ahead
    /// of the persistent finalised-state database, especially during sync or migration, so reporting
    /// source-derived finalised height would make routed callers observe a database height that has
    /// not actually been persisted.
    ///
    /// `None` means there is no persistent database height to report. This is the expected value
    /// when ephemeral is the real backend, for example in ephemeral mode. In that case
    /// [`DbRead::db_height`] reports zero.
    ///
    /// `Some(height)` means ephemeral is temporarily serving requests while a persistent backend
    /// exists elsewhere. Sync and migration code should update this value after successful
    /// persistent writes or rebuild progress so routed callers observe the actual on-disk database
    /// height.
    ///
    /// The value is stored behind [`Arc<RwLock<_>>`] so all clones share the same reported height and
    /// progress updates can be made safely from other threads.
    db_height: Arc<RwLock<Option<Height>>>,
}

impl<T: BlockchainSource> EphemeralFinalisedState<T> {
    pub(crate) fn new(
        source: T,
        network: zebra_chain::parameters::Network,
        db_height: Option<Height>,
    ) -> Self {
        let status = NamedAtomicStatus::new("ephemeral-finalised-state", StatusType::Spawning);

        let shutdown_requested = Arc::new(AtomicBool::new(false));

        let status_poll_source = source.clone();
        let status_poll_status = status.clone();
        let status_poll_shutdown_requested = Arc::clone(&shutdown_requested);

        let status_poll_task_handle = tokio::spawn(async move {
            loop {
                if status_poll_shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }

                let status = match status_poll_source.get_best_block_height().await {
                    Ok(_) => StatusType::Ready,
                    Err(_) => StatusType::CriticalError,
                };

                status_poll_status.store(status);

                tokio::select! {
                    _ = tokio::time::sleep(EPHEMERAL_FINALISED_STATE_STATUS_POLL_INTERVAL) => {}

                    _ = async {
                        while !status_poll_shutdown_requested.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    } => {
                        break;
                    }
                }
            }
        });

        Self {
            source,
            network,
            status,
            shutdown_requested,
            status_poll_task_handle: Arc::new(Mutex::new(Some(status_poll_task_handle))),
            db_height: Arc::new(RwLock::new(db_height)),
        }
    }

    /// Returns the persistent database height reported by this ephemeral backend.
    ///
    /// This value is independent of the backing source height. It is used when ephemeral
    /// is temporarily serving requests during sync or migration while the persistent
    /// database continues to progress separately.
    pub(crate) fn reported_db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        let db_height_guard = self.db_height.read().map_err(|error| {
            FinalisedStateError::Custom(format!(
                "ephemeral finalised state db height lock poisoned: {error}"
            ))
        })?;

        Ok(*db_height_guard)
    }

    /// Updates the persistent database height reported by this ephemeral backend.
    ///
    /// `None` means no persistent database height is available, which is the expected
    /// value when ephemeral is used as the real backend in ephemeral mode.
    pub(crate) fn update_db_height(
        &self,
        db_height: Option<Height>,
    ) -> Result<(), FinalisedStateError> {
        let mut db_height_guard = self.db_height.write().map_err(|error| {
            FinalisedStateError::Custom(format!(
                "ephemeral finalised state db height lock poisoned: {error}"
            ))
        })?;

        *db_height_guard = db_height;

        Ok(())
    }

    /// Stores a new runtime status for this ephemeral backend.
    ///
    /// This uses the same status hook exposed through [`DbCore::status`]. It is intended for router or
    /// backend-level orchestration code that needs to report a background failure through the existing
    /// database status path.
    pub(crate) fn store_status(&self, status: StatusType) {
        self.status.store(status);
    }

    fn feature_unavailable(feature_name: &'static str) -> FinalisedStateError {
        FinalisedStateError::FeatureUnavailable(feature_name)
    }

    async fn get_block_by_height(
        &self,
        height: Height,
    ) -> Result<Option<std::sync::Arc<zebra_chain::block::Block>>, FinalisedStateError> {
        self.source
            .get_block(HashOrHeight::Height(height.into()))
            .await
            .map_err(FinalisedStateError::from)
    }

    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<std::sync::Arc<zebra_chain::block::Block>>, FinalisedStateError> {
        self.source
            .get_block(HashOrHeight::Hash(hash.into()))
            .await
            .map_err(FinalisedStateError::from)
    }

    async fn get_required_block_by_height(
        &self,
        height: Height,
    ) -> Result<std::sync::Arc<zebra_chain::block::Block>, FinalisedStateError> {
        self.get_block_by_height(height).await?.ok_or_else(|| {
            FinalisedStateError::DataUnavailable(format!(
                "Error fetching block at height {height} from validator"
            ))
        })
    }

    async fn get_required_chain_block(
        &self,
        height: Height,
    ) -> Result<IndexedBlock, FinalisedStateError> {
        let block = self.get_required_block_by_height(height).await?;
        let block_hash = BlockHash::from(block.hash());
        let block_height = zebra_chain::block::Height(height.0);

        let (sapling, orchard) = self.source.get_commitment_tree_roots(block_hash).await?;
        let sapling_is_active = self.network.is_nu_active(
            zcash_protocol::consensus::NetworkUpgrade::Sapling,
            block_height.into(),
        );
        let orchard_is_active = self.network.is_nu_active(
            zcash_protocol::consensus::NetworkUpgrade::Nu5,
            block_height.into(),
        );
        let (sapling_root, sapling_size) = match sapling {
            Some((root, size)) => (root, size),
            None if !sapling_is_active => Default::default(),
            None => {
                return Err(FinalisedStateError::BlockchainSourceError(
                    BlockchainSourceError::Unrecoverable(format!(
                "missing Sapling commitment tree root for active Sapling block at height {height}"
            )),
                ));
            }
        };
        let (orchard_root, orchard_size) = match orchard {
            Some((root, size)) => (root, size),
            None if !orchard_is_active => Default::default(),
            None => {
                return Err(FinalisedStateError::BlockchainSourceError(
                    BlockchainSourceError::Unrecoverable(format!(
                "missing Orchard commitment tree root for active NU5 block at height {height}"
            )),
                ));
            }
        };
        let sapling_size = u32::try_from(sapling_size).map_err(|error| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                format!("sapling commitment tree size does not fit into u32: {error}"),
            ))
        })?;
        let orchard_size = u32::try_from(orchard_size).map_err(|error| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                format!("orchard commitment tree size does not fit into u32: {error}"),
            ))
        })?;

        let block_metadata = BlockMetadata::new(
            sapling_root,
            sapling_size,
            orchard_root,
            orchard_size,
            ChainWork::from(U256::zero()),
            self.network.clone(),
        );
        let block_with_metadata = BlockWithMetadata::new(block.as_ref(), block_metadata);
        let mut indexed_block = IndexedBlock::try_from(block_with_metadata).map_err(|error| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                format!("could not build indexed block from validator block: {error}"),
            ))
        })?;
        indexed_block.context = BlockContext::new(
            *indexed_block.hash(),
            *indexed_block.context.parent_hash(),
            ChainWork::from(U256::zero()),
            indexed_block.height(),
        );

        Ok(indexed_block)
    }
}

#[async_trait]
impl<T> DbCore for EphemeralFinalisedState<T>
where
    T: BlockchainSource + Clone + Send + Sync + 'static,
{
    /// Return the current status of the backend.
    ///
    /// This returns the latest status observed by the background status poll task.
    fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Shut down the backend and release associated resources.
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.shutdown_requested.store(true, Ordering::SeqCst);

        let status_poll_task_handle = {
            let mut status_poll_task_handle_guard = self.status_poll_task_handle.lock().await;

            status_poll_task_handle_guard.take()
        };

        if let Some(status_poll_task_handle) = status_poll_task_handle {
            status_poll_task_handle.abort();

            match status_poll_task_handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    return Err(FinalisedStateError::BlockchainSourceError(
                        BlockchainSourceError::Unrecoverable(format!(
                        "ephemeral finalised state status poll task failed during shutdown: {error}"
                    )),
                    ));
                }
            }
        }

        Ok(())
    }
}

impl<T> Drop for EphemeralFinalisedState<T>
where
    T: BlockchainSource,
{
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);

        if Arc::strong_count(&self.status_poll_task_handle) == 1 {
            if let Ok(mut status_poll_task_handle_guard) = self.status_poll_task_handle.try_lock() {
                if let Some(status_poll_task_handle) = status_poll_task_handle_guard.take() {
                    status_poll_task_handle.abort();
                }
            }
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> DbWrite for EphemeralFinalisedState<T> {
    /// Write a fully-indexed block into the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn write_block(&self, _block: IndexedBlock) -> Result<(), FinalisedStateError> {
        Ok(())
    }

    /// Delete the block at a given height, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block_at_height(&self, _height: Height) -> Result<(), FinalisedStateError> {
        Ok(())
    }

    /// Delete a specific indexed block from the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block(&self, _block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        Ok(())
    }

    /// Update the database metadata record.
    ///
    /// This is used by migrations and schema management logic.
    async fn update_metadata(&self, _metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        Ok(())
    }

    /// Bulk catch-up ingestion.
    ///
    /// No-op for the ephemeral passthrough: there is no persistent store to ingest into, and
    /// finalised reads are served straight from the backing source. `sync_to_height` short-circuits
    /// before reaching here when the primary is ephemeral; this satisfies the `DbWrite` contract.
    async fn write_blocks_to_height<S: BlockchainSource>(
        &self,
        _height: Height,
        _source: &S,
    ) -> Result<(), FinalisedStateError> {
        Ok(())
    }
}

#[async_trait]
impl<T: BlockchainSource> DbRead for EphemeralFinalisedState<T> {
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        Ok(Some(self.reported_db_height()?.unwrap_or(Height(0))))
    }

    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        let Some(block) = self.get_block_by_hash(hash).await? else {
            return Ok(None);
        };

        Ok(block.coinbase_height().map(Height::from))
    }

    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        let Some(block) = self.get_block_by_height(height).await? else {
            return Ok(None);
        };

        Ok(Some(BlockHash::from(block.hash())))
    }

    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        Err(Self::feature_unavailable(
            "READ_CORE:metadata requires an active DB",
        ))
    }
}

#[async_trait]
impl<T: BlockchainSource> BlockCoreExt for EphemeralFinalisedState<T> {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;
        Ok(BlockHeaderData::new(chain_block.context, chain_block.data))
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        let mut headers = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            headers.push(
                self.get_block_header(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(headers)
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        let block = self.get_required_block_by_height(height).await?;

        let txids = block
            .transactions
            .iter()
            .map(|transaction| TransactionHash::from(transaction.hash()))
            .collect();

        Ok(TxidList::new(txids))
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        let mut txid_lists = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            txid_lists.push(
                self.get_block_txids(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(txid_lists)
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        let block_height = Height::try_from(tx_location.block_height())
            .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;

        let tx_index = usize::from(tx_location.tx_index());
        let txids = self.get_block_txids(block_height).await?;

        txids.txids().get(tx_index).copied().ok_or_else(|| {
            FinalisedStateError::DataUnavailable(format!("transaction at location {tx_location:?}"))
        })
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        match self.source.get_transaction(*txid).await? {
            Some((_transaction, GetTransactionLocation::BestChain(height))) => {
                let block_height = Height::from(height);
                let txids = self.get_block_txids(block_height).await?;

                let Some(tx_index) = txids
                    .txids()
                    .iter()
                    .position(|candidate_txid| candidate_txid == txid)
                else {
                    return Ok(None);
                };

                let tx_index = u16::try_from(tx_index)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;

                Ok(Some(TxLocation::new(u32::from(block_height), tx_index)))
            }
            Some((
                _transaction,
                GetTransactionLocation::Mempool | GetTransactionLocation::NonbestChain,
            )) => Ok(None),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> BlockTransparentExt for EphemeralFinalisedState<T> {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        let chain_block = self
            .get_required_chain_block(Height::try_from(tx_location.block_height()).unwrap())
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.transparent().clone()))
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;

        Ok(TransparentTxList::new(
            chain_block
                .transactions()
                .iter()
                .map(|transaction| Some(transaction.transparent().clone()))
                .collect(),
        ))
    }

    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        let mut transparent_lists = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            transparent_lists.push(
                self.get_block_transparent(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(transparent_lists)
    }

    async fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, FinalisedStateError> {
        let previous_transaction_hash = TransactionHash(*outpoint.prev_txid());

        let Some(previous_transaction_location) =
            self.get_tx_location(&previous_transaction_hash).await?
        else {
            return Err(FinalisedStateError::DataUnavailable(format!(
                "previous transaction not found for outpoint {outpoint:?}"
            )));
        };

        let Some(previous_transaction_transparent_data) =
            self.get_transparent(previous_transaction_location).await?
        else {
            return Err(FinalisedStateError::DataUnavailable(format!(
                "previous transaction has no transparent data for outpoint {outpoint:?}"
            )));
        };

        previous_transaction_transparent_data
            .outputs()
            .get(usize::try_from(outpoint.prev_index()).map_err(|error| {
                FinalisedStateError::Custom(format!(
                    "outpoint output index does not fit into usize: {error}"
                ))
            })?)
            .copied()
            .ok_or_else(|| {
                FinalisedStateError::DataUnavailable(format!(
                    "previous output index {} not found in transaction {:?}",
                    outpoint.prev_index(),
                    previous_transaction_hash,
                ))
            })
    }
}

#[async_trait]
impl<T: BlockchainSource> BlockShieldedExt for EphemeralFinalisedState<T> {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        let chain_block = self
            .get_required_chain_block(Height::try_from(tx_location.block_height()).unwrap())
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.sapling().clone()))
    }

    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;

        Ok(SaplingTxList::new(
            chain_block
                .transactions()
                .iter()
                .map(|transaction| Some(transaction.sapling().clone()))
                .collect(),
        ))
    }

    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        let mut sapling_lists = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            sapling_lists.push(
                self.get_block_sapling(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(sapling_lists)
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        let chain_block = self
            .get_required_chain_block(Height::try_from(tx_location.block_height()).unwrap())
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.orchard().clone()))
    }

    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;

        Ok(OrchardTxList::new(
            chain_block
                .transactions()
                .iter()
                .map(|transaction| Some(transaction.orchard().clone()))
                .collect(),
        ))
    }

    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        let mut orchard_lists = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            orchard_lists.push(
                self.get_block_orchard(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(orchard_lists)
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;
        Ok(*chain_block.commitment_tree_data())
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        let mut commitment_tree_data = Vec::new();

        for height in u32::from(start)..=u32::from(end) {
            commitment_tree_data.push(
                self.get_block_commitment_tree_data(Height::try_from(height).unwrap())
                    .await?,
            );
        }

        Ok(commitment_tree_data)
    }
}

#[async_trait]
impl<T: BlockchainSource> CompactBlockExt for EphemeralFinalisedState<T> {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        let chain_block = self.get_required_chain_block(height).await?;
        Ok(compact_block_with_pool_types(
            chain_block.to_compact_block(),
            &pool_types.to_pool_types_vector(),
        ))
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        let (compact_block_sender, compact_block_receiver) =
            tokio::sync::mpsc::channel::<Result<CompactBlock, tonic::Status>>(32);

        let source = self.clone();

        tokio::spawn(async move {
            for height in start_height.0..=end_height.0 {
                let height = match Height::try_from(height) {
                    Ok(height) => height,
                    Err(_error) => {
                        let _ = compact_block_sender
                            .send(Err(tonic::Status::out_of_range(
                                "Invalid height range".to_string(),
                            )))
                            .await;
                        break;
                    }
                };

                let compact_block_result = source
                    .get_compact_block(height, pool_types.clone())
                    .await
                    .map_err(|error| tonic::Status::internal(error.to_string()));

                if compact_block_sender
                    .send(compact_block_result)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(CompactBlockStream::new(compact_block_receiver))
    }
}

#[async_trait]
impl<T: BlockchainSource> IndexedBlockExt for EphemeralFinalisedState<T> {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        match self.get_required_chain_block(height).await {
            Ok(chain_block) => Ok(Some(chain_block)),
            Err(FinalisedStateError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
