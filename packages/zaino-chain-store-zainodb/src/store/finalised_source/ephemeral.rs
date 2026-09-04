//! EphemeralFinalisedState provides access to the finalised portion of the
//! chain when the FinalisedState is syncing, migrating, or switched off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Mutex;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_status::{NamedAtomicStatus, StatusType};
use zcash_protocol::consensus::Parameters as _;

use crate::pool::ShieldedPool;
use crate::store::capability::{DbCore, DbWrite};
use crate::store::DbMetadata;

use super::super::{indexed_block_from_parts, require_pool_roots, PoolActivation};
use crate::error::{source_error, StoreError};
use crate::store::capability::{
    BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbRead, IndexedBlockExt,
};
use crate::stream::CompactBlockStream;
use crate::types::{
    BlockHash, BlockHeaderData, CommitmentTreeData, Height, IndexedBlock, OrchardCompactTx,
    OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList, TransactionHash,
    TransparentCompactTx, TransparentTxList, TxLocation, TxOutCompact, TxidList,
};
use zaino_chain_store::ChainStoreSource;

use zaino_proto::proto::utils::PoolTypeFilter;

const EPHEMERAL_FINALISED_STATE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Converts a raw `u32` block height (a stored [`TxLocation`] height) into a [`Height`],
/// surfacing an out-of-range value as a [`StoreError`] instead of panicking.
fn height_from_u32(height: u32) -> Result<Height, StoreError> {
    Height::try_from(height)
        .map_err(|error| StoreError::Custom(format!("invalid block height {height}: {error}")))
}

/// This crate's height, as the domain names it.
///
/// The stored height is any `u32`; the domain's is validated against the
/// protocol maximum. A height that cannot be expressed is surfaced as an error
/// rather than clamped, because clamping would silently answer about a
/// different block.
fn domain_height(height: Height) -> Result<zaino_primitives::types::Height, StoreError> {
    zaino_primitives::types::Height::try_from(height.0)
        .map_err(|error| StoreError::Custom(format!("invalid block height {height}: {error}")))
}

/// Collects one item per height across the inclusive `start..=end` range, in ascending
/// order, by calling `get_at` for each height.
async fn collect_block_range<T, Fut>(
    start: Height,
    end: Height,
    mut get_at: impl FnMut(Height) -> Fut,
) -> Result<Vec<T>, StoreError>
where
    Fut: std::future::Future<Output = Result<T, StoreError>>,
{
    let mut items = Vec::new();
    for height in Height::range_inclusive(start, end) {
        items.push(get_at(height).await?);
    }
    Ok(items)
}

/// Source-backed finalised-state backend used when persistent finalised-state storage is not
/// serving normal requests.
///
/// `EphemeralFinalisedState` does not own or mutate an on-disk database. Instead, it answers
/// finalised-state read requests by querying the backing [`ChainStoreSource`] directly and building
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
#[derive(Debug)]
pub(crate) struct EphemeralFinalisedState<T: ChainStoreSource> {
    /// Backing blockchain source used to answer finalised-state reads.
    ///
    /// This is typically a validator/source service. Ephemeral read methods fetch blocks,
    /// transactions, commitment tree data, and chain metadata from this source and convert them into
    /// the same response types exposed by persistent database backends.
    source: Arc<T>,

    /// Network whose consensus rules are used when reconstructing finalised-state data.
    ///
    /// This is required for network-upgrade checks, especially when deciding whether Sapling or
    /// Orchard commitment tree data is expected for a block.
    network: zebra_chain::parameters::Network,

    /// Current runtime status of the ephemeral backend.
    ///
    /// The background status-poll task updates this value by periodically checking whether the
    /// backing [`ChainStoreSource`] is reachable. [`DbCore::status`] returns this value directly.
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

/// Cloned by hand rather than derived.
///
/// A derived `Clone` would require `T: Clone`, which a validator is not — it
/// may own connections that must not be duplicated. Every field here is either
/// shared behind an [`Arc`] or trivially copied, so the bound is unnecessary.
impl<T: ChainStoreSource> Clone for EphemeralFinalisedState<T> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            network: self.network.clone(),
            status: self.status.clone(),
            shutdown_requested: Arc::clone(&self.shutdown_requested),
            status_poll_task_handle: Arc::clone(&self.status_poll_task_handle),
            db_height: Arc::clone(&self.db_height),
        }
    }
}

impl<T: ChainStoreSource> EphemeralFinalisedState<T> {
    pub(crate) fn new(
        source: Arc<T>,
        network: zebra_chain::parameters::Network,
        db_height: Option<Height>,
    ) -> Self {
        let status = NamedAtomicStatus::new("ephemeral-finalised-state", StatusType::Spawning);

        let shutdown_requested = Arc::new(AtomicBool::new(false));

        let status_poll_source = Arc::clone(&source);
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
    pub(crate) fn reported_db_height(&self) -> Result<Option<Height>, StoreError> {
        let db_height_guard = self.db_height.read().map_err(|error| {
            StoreError::Custom(format!(
                "ephemeral finalised state db height lock poisoned: {error}"
            ))
        })?;

        Ok(*db_height_guard)
    }

    /// Updates the persistent database height reported by this ephemeral backend.
    ///
    /// `None` means no persistent database height is available, which is the expected
    /// value when ephemeral is used as the real backend in ephemeral mode.
    pub(crate) fn update_db_height(&self, db_height: Option<Height>) -> Result<(), StoreError> {
        let mut db_height_guard = self.db_height.write().map_err(|error| {
            StoreError::Custom(format!(
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

    /// The concrete v1 backend a read needs is absent because this backend is
    /// the ephemeral migration passthrough. A backend-state refusal, not a
    /// capability one; `handle` names what was asked for, for the operator's log.
    fn v1_backend_unavailable(handle: &'static str) -> StoreError {
        StoreError::V1BackendUnavailable(handle)
    }

    /// The block at `height`, or `None` when the validator has no such block.
    ///
    /// A domain-level rejection is a miss, not a failure: the validator
    /// answered, and the answer was "no such block". A transport failure is
    /// propagated, because it says nothing about whether the block exists.
    async fn get_block_by_height(
        &self,
        height: Height,
    ) -> Result<Option<zaino_primitives::types::Block>, StoreError> {
        let height = domain_height(height)?;
        match self.source.get_block(height).await {
            Ok(block) => Ok(Some(block)),
            Err(zaino_source::QueryError::Domain(_)) => Ok(None),
            Err(error) => Err(StoreError::Source(source_error(error))),
        }
    }

    /// The block with `hash`, or `None` when the validator has no such block.
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<zaino_primitives::types::Block>, StoreError> {
        match self
            .source
            .get_block_by_hash(zaino_primitives::types::BlockHash::from(hash.0))
            .await
        {
            Ok(block) => Ok(Some(block)),
            Err(zaino_source::QueryError::Domain(_)) => Ok(None),
            Err(error) => Err(StoreError::Source(source_error(error))),
        }
    }

    async fn get_required_block_by_height(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::Block, StoreError> {
        self.get_block_by_height(height).await?.ok_or_else(|| {
            StoreError::DataUnavailable(format!(
                "Error fetching block at height {height} from validator"
            ))
        })
    }

    async fn get_required_chain_block(&self, height: Height) -> Result<IndexedBlock, StoreError> {
        let block = self.get_required_block_by_height(height).await?;
        let block_height = zebra_chain::block::Height(height.0);

        let tree_roots = self
            .source
            .get_commitment_tree_roots(block.header.hash)
            .await
            .map_err(|error| StoreError::Source(source_error(error)))?;

        let is_active = |pool: ShieldedPool| {
            self.network.is_nu_active(
                pool.zcash_protocol_activation_upgrade(),
                block_height.into(),
            )
        };

        require_pool_roots(
            &tree_roots,
            PoolActivation {
                sapling: is_active(ShieldedPool::Sapling),
                orchard: is_active(ShieldedPool::Orchard),
                ironwood: is_active(ShieldedPool::Ironwood),
            },
            block.header.hash,
        )?;

        // No chainwork: the ephemeral backend has no tip to accumulate from, so
        // each block carries only its own work. Blocks built here are served,
        // never written, so the value never reaches disk.
        indexed_block_from_parts(&block, &tree_roots, None)
    }
}

impl<T> DbCore for EphemeralFinalisedState<T>
where
    T: ChainStoreSource + Send + Sync + 'static,
{
    /// Return the current status of the backend.
    ///
    /// This returns the latest status observed by the background status poll task.
    fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Shut down the backend and release associated resources.
    async fn shutdown(&self) -> Result<(), StoreError> {
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
                    return Err(StoreError::Critical(format!(
                        "ephemeral finalised state status poll task failed during shutdown: {error}"
                    )));
                }
            }
        }

        Ok(())
    }
}

impl<T> Drop for EphemeralFinalisedState<T>
where
    T: ChainStoreSource,
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

impl<T: ChainStoreSource> DbWrite for EphemeralFinalisedState<T> {
    /// Write a fully-indexed block into the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn write_block(&self, _block: IndexedBlock) -> Result<(), StoreError> {
        Ok(())
    }

    /// Delete the block at a given height, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block_at_height(&self, _height: Height) -> Result<(), StoreError> {
        Ok(())
    }

    /// Delete a specific indexed block from the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block(&self, _block: &IndexedBlock) -> Result<(), StoreError> {
        Ok(())
    }

    /// Update the database metadata record.
    ///
    /// This is used by migrations and schema management logic.
    async fn update_metadata(&self, _metadata: DbMetadata) -> Result<(), StoreError> {
        Ok(())
    }

    /// Bulk catch-up ingestion.
    ///
    /// No-op for the ephemeral passthrough: there is no persistent store to ingest into, and
    /// finalised reads are served straight from the backing source. `sync_to_height` short-circuits
    /// before reaching here when the primary is ephemeral; this satisfies the `DbWrite` contract.
    async fn write_blocks_to_height<S: ChainStoreSource>(
        &self,
        _height: Height,
        _source: &S,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

impl<T: ChainStoreSource> DbRead for EphemeralFinalisedState<T> {
    async fn db_height(&self) -> Result<Option<Height>, StoreError> {
        Ok(Some(self.reported_db_height()?.unwrap_or(Height(0))))
    }

    async fn get_block_height(&self, hash: BlockHash) -> Result<Option<Height>, StoreError> {
        let Some(block) = self.get_block_by_hash(hash).await? else {
            return Ok(None);
        };

        Ok(Some(Height(u32::from(block.header.height))))
    }

    async fn get_block_hash(&self, height: Height) -> Result<Option<BlockHash>, StoreError> {
        let Some(block) = self.get_block_by_height(height).await? else {
            return Ok(None);
        };

        Ok(Some(BlockHash(block.header.hash.into())))
    }

    async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        Err(Self::v1_backend_unavailable(
            "metadata read requires an active v1 backend",
        ))
    }
}

impl<T: ChainStoreSource> BlockCoreExt for EphemeralFinalisedState<T> {
    async fn get_block_header(&self, height: Height) -> Result<BlockHeaderData, StoreError> {
        let chain_block = self.get_required_chain_block(height).await?;
        Ok(BlockHeaderData::new(chain_block.context, chain_block.data))
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_header(height)).await
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, StoreError> {
        let block = self.get_required_block_by_height(height).await?;

        let txids = block
            .transactions
            .iter()
            .map(|transaction| TransactionHash(transaction.txid.into()))
            .collect();

        Ok(TxidList::new(txids))
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_txids(height)).await
    }

    async fn get_txid(&self, tx_location: TxLocation) -> Result<TransactionHash, StoreError> {
        let block_height = Height::try_from(tx_location.block_height())
            .map_err(|error| StoreError::Custom(error.to_string()))?;

        let tx_index = usize::from(tx_location.tx_index());
        let txids = self.get_block_txids(block_height).await?;

        txids.txids().get(tx_index).copied().ok_or_else(|| {
            StoreError::DataUnavailable(format!("transaction at location {tx_location:?}"))
        })
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, StoreError> {
        let located = match self
            .source
            .get_transaction(zaino_primitives::types::TransactionId::from(txid.0))
            .await
        {
            Ok(response) => Some(response.location),
            Err(zaino_source::QueryError::Domain(_)) => None,
            Err(error) => return Err(StoreError::Source(source_error(error))),
        };

        match located {
            Some(zaino_primitives::types::TransactionLocation::BestChain(height)) => {
                let block_height = Height(u32::from(height));
                let txids = self.get_block_txids(block_height).await?;

                let Some(tx_index) = txids
                    .txids()
                    .iter()
                    .position(|candidate_txid| candidate_txid == txid)
                else {
                    return Ok(None);
                };

                let tx_index = u16::try_from(tx_index)
                    .map_err(|error| StoreError::Custom(error.to_string()))?;

                Ok(Some(TxLocation::new(u32::from(block_height), tx_index)))
            }
            Some(
                zaino_primitives::types::TransactionLocation::Mempool
                | zaino_primitives::types::TransactionLocation::NonBestChain,
            )
            | None => Ok(None),
        }
    }
}

impl<T: ChainStoreSource> BlockTransparentExt for EphemeralFinalisedState<T> {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, StoreError> {
        let chain_block = self
            .get_required_chain_block(height_from_u32(tx_location.block_height())?)
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.transparent().clone()))
    }

    async fn get_block_transparent(&self, height: Height) -> Result<TransparentTxList, StoreError> {
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
    ) -> Result<Vec<TransparentTxList>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_transparent(height)).await
    }

    async fn get_previous_output(&self, outpoint: Outpoint) -> Result<TxOutCompact, StoreError> {
        let previous_transaction_hash = TransactionHash(*outpoint.prev_txid());

        let Some(previous_transaction_location) =
            self.get_tx_location(&previous_transaction_hash).await?
        else {
            return Err(StoreError::DataUnavailable(format!(
                "previous transaction not found for outpoint {outpoint:?}"
            )));
        };

        let Some(previous_transaction_transparent_data) =
            self.get_transparent(previous_transaction_location).await?
        else {
            return Err(StoreError::DataUnavailable(format!(
                "previous transaction has no transparent data for outpoint {outpoint:?}"
            )));
        };

        previous_transaction_transparent_data
            .outputs()
            .get(usize::try_from(outpoint.prev_index()).map_err(|error| {
                StoreError::Custom(format!(
                    "outpoint output index does not fit into usize: {error}"
                ))
            })?)
            .copied()
            .ok_or_else(|| {
                StoreError::DataUnavailable(format!(
                    "previous output index {} not found in transaction {:?}",
                    outpoint.prev_index(),
                    previous_transaction_hash,
                ))
            })
    }
}

impl<T: ChainStoreSource> BlockShieldedExt for EphemeralFinalisedState<T> {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, StoreError> {
        let chain_block = self
            .get_required_chain_block(height_from_u32(tx_location.block_height())?)
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.sapling().clone()))
    }

    async fn get_block_sapling(&self, height: Height) -> Result<SaplingTxList, StoreError> {
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
    ) -> Result<Vec<SaplingTxList>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_sapling(height)).await
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, StoreError> {
        let chain_block = self
            .get_required_chain_block(height_from_u32(tx_location.block_height())?)
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.orchard().clone()))
    }

    async fn get_block_orchard(&self, height: Height) -> Result<OrchardTxList, StoreError> {
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
    ) -> Result<Vec<OrchardTxList>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_orchard(height)).await
    }

    async fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, StoreError> {
        let chain_block = self
            .get_required_chain_block(height_from_u32(tx_location.block_height())?)
            .await?;

        Ok(chain_block
            .transactions()
            .get(usize::from(tx_location.tx_index()))
            .map(|transaction| transaction.ironwood().clone()))
    }

    async fn get_block_ironwood(&self, height: Height) -> Result<OrchardTxList, StoreError> {
        let chain_block = self.get_required_chain_block(height).await?;

        Ok(OrchardTxList::new(
            chain_block
                .transactions()
                .iter()
                .map(|transaction| Some(transaction.ironwood().clone()))
                .collect(),
        ))
    }

    async fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, StoreError> {
        collect_block_range(start, end, |height| self.get_block_ironwood(height)).await
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, StoreError> {
        let chain_block = self.get_required_chain_block(height).await?;
        Ok(*chain_block.commitment_tree_data())
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, StoreError> {
        collect_block_range(start, end, |height| {
            self.get_block_commitment_tree_data(height)
        })
        .await
    }
}

impl<T: ChainStoreSource> CompactBlockExt for EphemeralFinalisedState<T> {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> Result<zaino_primitives::types::CompactBlock, StoreError> {
        let chain_block = self.get_required_chain_block(height).await?;
        crate::store::finalised_source::v1::compact_block::compact_block_from_indexed(
            &chain_block,
            pool_types,
        )
    }

    async fn get_compact_block_range(
        &self,
        start: Height,
        end: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<zaino_primitives::types::CompactBlock>, StoreError> {
        collect_block_range(start, end, |height| async move {
            let block = self.get_required_chain_block(height).await?;
            crate::store::finalised_source::v1::compact_block::compact_block_from_indexed(
                &block, pool_types,
            )
        })
        .await
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, StoreError> {
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
                    .get_compact_block(
                        height,
                        crate::store::finalised_source::v1::compact_block::pool_filter_from_wire(
                            &pool_types,
                        ),
                    )
                    .await
                    .map(|block| {
                        crate::store::finalised_source::v1::compact_block::compact_block_to_wire(
                            &block,
                        )
                    })
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

impl<T: ChainStoreSource> IndexedBlockExt for EphemeralFinalisedState<T> {
    async fn get_chain_block(&self, height: Height) -> Result<Option<IndexedBlock>, StoreError> {
        match self.get_required_chain_block(height).await {
            Ok(chain_block) => Ok(Some(chain_block)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// One validator round trip per height.
    ///
    /// No batching to be had: the passthrough has no transaction to hold open
    /// and no local rows to walk, so a range is exactly its blocks fetched one
    /// after another. This is why passthrough is a stopgap and not a mode to
    /// serve a large range from.
    async fn get_chain_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<IndexedBlock>, StoreError> {
        collect_block_range(start, end, |height| self.get_required_chain_block(height)).await
    }
}
