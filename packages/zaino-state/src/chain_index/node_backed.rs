//! The chain index: the chain view and the mempool, launched and held together.

use std::str::FromStr as _;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zaino_chain::{
    BlockRead as _, ChainViewSnapshot as _, ChainViewSync, ForkReconcile as _, Locator,
};
use zaino_chain_store::ChainStoreService as _;
use zaino_chain_store_zainodb::store::{reader::DbReader, FinalisedState, FinalisedStateMode};
use zaino_component::{ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_status::{NamedAtomicStatus, Status, StatusType};
use zebra_state::HashOrHeight;

use super::chain_head::{self, WithChainHeadSource};
use super::chain_store::{self, WithChainStoreSource};
use super::chain_view::{self, ChainIndexSnapshot, Composer, WithChainViewSource};
use super::mempool::{self, ChainIndexCoherence, ChainIndexMempool};
use super::source::BlockchainSource;
use super::types;
use super::ZebraNetwork;
use crate::error::ChainIndexError;

/// Folds the statuses of the index's components into the index's own.
///
/// Components are read live on every call, so a component that recovers is
/// reported as recovered.
pub(crate) fn combine_component_statuses(
    own: StatusType,
    finalised: StatusType,
    mempool: StatusType,
    chain_head: StatusType,
) -> StatusType {
    own.combine(finalised).combine(mempool).combine(chain_head)
}

/// A component's two axes, back as the one status this index folds.
///
/// Health first, because that is the axis `combine` ranks.
fn fused(status: ComponentStatus) -> StatusType {
    match status.health {
        Health::Critical => StatusType::CriticalError,
        Health::Recoverable => StatusType::RecoverableError,
        Health::Offline => StatusType::Offline,
        Health::Healthy => match status.lifecycle {
            Lifecycle::Offline => StatusType::Offline,
            Lifecycle::Spawning => StatusType::Spawning,
            Lifecycle::Syncing => StatusType::Syncing,
            Lifecycle::Ready => StatusType::Ready,
            Lifecycle::Closing => StatusType::Closing,
        },
    }
}

/// The finalised store's status, with the health of the sync feeding it.
fn store_status(store: ComponentStatus, sync: Option<&ChainViewSync>) -> StatusType {
    let store = fused(store);
    match sync.map(|sync| sync.status().health) {
        Some(Health::Critical) => store.combine(StatusType::CriticalError),
        Some(Health::Recoverable) => store.combine(StatusType::RecoverableError),
        _ => store,
    }
}

/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once.
///
/// This is the primary implementation backing [`ChainIndex`](super::ChainIndex).
/// It can be backed by either:
/// - A zebra `ReadStateService` for direct database access (preferred for performance)
/// - A JSON-RPC connection to any validator node (zebrad or another zainod)
///
/// To use the [`ChainIndex`](super::ChainIndex) trait methods, call
/// [`subscriber()`](NodeBackedChainIndex::subscriber) to get a
/// [`NodeBackedChainIndexSubscriber`] which implements the trait.
///
/// # Construction
///
/// Use [`NodeBackedChainIndex::new()`] with:
/// - A source implementing [`BlockchainSource`] — in production
///   [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
///   built by `spawn_rpc` or `spawn_direct` from the backend config
/// - A [`ChainIndexConfig`](crate::ChainIndexConfig) containing cache and database settings
///
/// Most consumers should not build one directly:
/// [`NodeBackedIndexerService`](crate::NodeBackedIndexerService) does it from
/// config, and additionally waits for the initial sync to complete.
pub struct NodeBackedChainIndex<
    Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource = crate::chain_index::validator_source::ZebraValidatorSource,
> {
    composer: Arc<Composer<Source>>,
    /// `None` for an ephemeral store, which has nothing to freeze into.
    chain_view_sync: Option<Arc<ChainViewSync>>,
    finalized_db: FinalisedState<Source::Store>,
    chain_head: Arc<zaino_chain_head_service::ChainHeadService<Source::Head>>,
    mempool: Arc<ChainIndexMempool<Source>>,
    coherence: Arc<ChainIndexCoherence>,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: Source,
    /// Stops every task this index started.
    cancel_token: CancellationToken,
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > NodeBackedChainIndex<Source>
{
    /// Creates a new chainindex from a connection to a validator.
    ///
    /// In production `Source` is
    /// [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
    /// which routes each query to Zebra's read-state service or its JSON-RPC
    /// interface according to which can answer it.
    pub async fn new(
        source: Source,
        config: crate::config::ChainIndexConfig,
    ) -> Result<Self, crate::InitError> {
        Self::new_with_chain_head_config(source, config, chain_head::config()).await
    }

    /// Like [`Self::new`] but with the chain head's timings overridden, for
    /// tests that need a faster failure ladder.
    pub(crate) async fn new_with_chain_head_config(
        source: Source,
        config: crate::config::ChainIndexConfig,
        chain_head_config: zaino_chain_head::ChainHeadConfig,
    ) -> Result<Self, crate::InitError> {
        let finalized_db = chain_store::spawn(&config, &source).await?;
        let cancel_token = CancellationToken::new();
        let chain_head = chain_head::spawn(&source, chain_head_config, cancel_token.child_token())
            .await
            .map_err(crate::InitError::ChainHeadInitialisationError)?;
        let (mempool, coherence) = mempool::spawn(
            &source,
            chain_head.subscriber(),
            &config.mempool,
            &cancel_token,
        );
        let composer = chain_view::compose(finalized_db.clone(), chain_head.subscriber(), &source);
        let chain_view_sync =
            (!config.ephemeral).then(|| composer.spawn_sync(cancel_token.child_token()));

        Ok(Self {
            composer,
            chain_view_sync,
            finalized_db,
            chain_head,
            mempool,
            coherence,
            status: NamedAtomicStatus::new("ChainIndex", StatusType::Ready),
            network: config.network.clone(),
            source,
            cancel_token,
        })
    }

    /// The finalised store's watermark, for tests that watch the seam advance.
    #[cfg(test)]
    pub(crate) fn finalised_watermark(&self) -> zaino_chain_store::StoreWatermark {
        zaino_chain_store::ChainStoreReader::watermark(&self.finalized_db.reader())
    }

    /// Creates a [`NodeBackedChainIndexSubscriber`] from self,
    /// a clone-safe, drop-safe, read-only view onto the running indexer.
    pub fn subscriber(&self) -> NodeBackedChainIndexSubscriber<Source> {
        NodeBackedChainIndexSubscriber {
            composer: Arc::clone(&self.composer),
            chain_view_sync: self.chain_view_sync.clone(),
            finalized_state: self.finalized_db.reader(),
            chain_head: self.chain_head.subscriber(),
            mempool: self.mempool.subscriber(),
            coherence: self.coherence.subscriber(),
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }

    /// Shut down the index, for a cleaner drop.
    /// An error indicates a failure to cleanly shutdown. Dropping the
    /// chain index should still stop everything.
    pub async fn shutdown(&self) -> Result<(), zaino_chain_store::ChainStoreError> {
        // The synchronous teardown runs before the fallible store shutdown so a
        // store error cannot skip it — the source's Zebra syncer task must not
        // outlive the index.
        self.shutdown_sync_best_effort();
        zaino_chain_store::ChainStoreIngest::shutdown(&self.finalized_db).await
    }

    /// Synchronous best-effort teardown for contexts that cannot run async
    /// work (a `Drop` on a current-thread runtime, or on a thread with no
    /// runtime at all). The store's async shutdown is skipped; its own `Drop`
    /// releases its resources.
    pub(crate) fn shutdown_sync_best_effort(&self) {
        self.cancel_token.cancel();
        self.status.store(StatusType::Closing);
        self.mempool.close();
        self.chain_head.shutdown();
        if let Some(sync) = &self.chain_view_sync {
            sync.shutdown();
        }
        self.source.shutdown();
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.coherence.subscriber().frozen_for()
    }

    /// Returns which backend is currently answering finalised-state reads.
    ///
    /// Companion to [`NodeBackedChainIndex::status`], which cannot express this: an ephemeral
    /// passthrough reports [`StatusType::Ready`] identically to a synced persistent database.
    pub fn finalised_state_mode(&self) -> FinalisedStateMode {
        self.finalized_db.finalised_state_mode()
    }

    /// Displays the status of the chain_index
    pub fn status(&self) -> StatusType {
        combine_component_statuses(
            self.status.load(),
            store_status(
                StatusSource::status(&self.finalized_db),
                self.chain_view_sync.as_deref(),
            ),
            self.mempool.status(),
            fused(self.chain_head.status()),
        )
    }
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > Drop for NodeBackedChainIndex<Source>
{
    /// The full synchronous teardown, so the source's syncer task does not
    /// outlive an index dropped without an explicit `shutdown()`.
    fn drop(&mut self) {
        self.shutdown_sync_best_effort();
    }
}

/// Formatted by hand: a derive would demand `Debug` of the validator types.
impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > std::fmt::Debug for NodeBackedChainIndex<Source>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBackedChainIndex")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// A clone-safe *read-only* view onto a running [`NodeBackedChainIndex`].
///
/// [`NodeBackedChainIndexSubscriber`] can safely be cloned and dropped freely.
pub struct NodeBackedChainIndexSubscriber<
    Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource = crate::chain_index::validator_source::ZebraValidatorSource,
> {
    pub(super) composer: Arc<Composer<Source>>,
    chain_view_sync: Option<Arc<ChainViewSync>>,
    /// Held for its status only; reads go through the chain view.
    finalized_state: DbReader<Source::Store>,
    /// Held for its status only; reads go through the chain view.
    chain_head: zaino_chain_head_service::ChainHeadSubscriber,
    /// The live set: answers regardless of what the tip is doing.
    pub(super) mempool: zaino_mempool_service::MempoolSubscriber,
    /// The tip-coherent view over it, for the reads that place a transaction
    /// relative to a tip.
    pub(super) coherence: zaino_mempool_service::CoherentSubscriber,
    status: NamedAtomicStatus,
    pub(super) network: ZebraNetwork,
    source: Source,
}

/// Cloned by hand: a derive would demand `Clone` of the validator types, which
/// are shared behind `Arc`s here.
impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > Clone for NodeBackedChainIndexSubscriber<Source>
{
    fn clone(&self) -> Self {
        Self {
            composer: Arc::clone(&self.composer),
            chain_view_sync: self.chain_view_sync.clone(),
            finalized_state: self.finalized_state.clone(),
            chain_head: self.chain_head.clone(),
            mempool: self.mempool.clone(),
            coherence: self.coherence.clone(),
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > std::fmt::Debug for NodeBackedChainIndexSubscriber<Source>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBackedChainIndexSubscriber")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > NodeBackedChainIndexSubscriber<Source>
{
    pub(crate) fn indexed_tip_stream(&self) -> crate::IndexedTipStream {
        crate::stream::indexed_tip_stream(zaino_chain::ChainView::subscribe_tip(
            self.composer.as_ref(),
        ))
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The indexer's mempool subscriber.
    ///
    /// Test-only escape hatch. Production code goes through the `ChainIndex`
    /// mempool API.
    #[cfg(feature = "test_dependencies")]
    pub(crate) fn mempool_subscriber(&self) -> &zaino_mempool_service::MempoolSubscriber {
        &self.mempool
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    ///
    /// A brief non-`None` value is the normal shape of a tip transition. A
    /// sustained one means the validator tip and Zaino's have stopped agreeing,
    /// and every tip-coherent read is failing.
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.coherence.frozen_for()
    }

    /// Returns the combined status of all chain index components.
    pub fn combined_status(&self) -> StatusType {
        combine_component_statuses(
            self.status.load(),
            store_status(
                StatusSource::status(&self.finalized_state),
                self.chain_view_sync.as_deref(),
            ),
            self.mempool.status(),
            fused(self.chain_head.status()),
        )
    }

    /// The epoch the mempool is coherent with, if it is the one `snapshot`
    /// was taken at.
    pub(super) fn coherent_epoch(
        coherent: &zaino_mempool::CoherentSnapshot,
        snapshot: &ChainIndexSnapshot<Source>,
    ) -> Option<zaino_primitives::types::ChainStateEpoch> {
        Some(snapshot.epoch()).filter(|epoch| coherent.is_valid_for_snapshot(*epoch))
    }

    /// Whether the chain head (on any branch) or the store holds `hash`.
    pub(super) async fn block_hash_known(
        snapshot: &ChainIndexSnapshot<Source>,
        hash: types::BlockHash,
    ) -> Result<bool, ChainIndexError> {
        Ok(snapshot
            .fork_point(&Locator::new(vec![types::domain_hash(hash)]))
            .await?
            .is_some())
    }

    /// Whether the hash-or-height string names a block this index knows.
    pub(crate) async fn hash_or_height_known_for_treestate(
        &self,
        snapshot: &ChainIndexSnapshot<Source>,
        hash_or_height: &str,
    ) -> Result<bool, ChainIndexError> {
        let hash_or_height = HashOrHeight::from_str(hash_or_height).map_err(|error| {
            ChainIndexError::internal(format!("invalid hash or height: {error}"))
        })?;
        let known = match hash_or_height {
            HashOrHeight::Hash(hash) => Self::block_hash_known(snapshot, hash.into()).await?,
            HashOrHeight::Height(height) => match types::domain_height(height.into()) {
                Some(height) => snapshot.block_hash(height).await?.is_some(),
                None => false,
            },
        };
        Ok(known)
    }
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > Status for NodeBackedChainIndexSubscriber<Source>
{
    fn status(&self) -> StatusType {
        self.combined_status()
    }
}
