//! Zaino's implementation of the ChainView ports.
//!
//! # The mechanism
//!
//! `Coverage` says who answers which heights; every read asks it and then
//! asks the provider it named. A point read is one segment, a range read is
//! several stitched in height order, and there is nothing between the reads and
//! that decision.
//!
//! # Generic over ports, not implementations
//!
//! The parameters are bounded by [`ChainStoreService`],
//! [`ChainHeadBlockService`] and [`ChainViewSource`] — ports, all three. No
//! adapter is named here, and each read trait is implemented under *its own*
//! bounds, so a store that builds only a compact index yields a view offering
//! compact reads and genuinely not offering spend status. Mixing this crate's
//! composer with someone else's store, or someone else's composer with these
//! ports, is the same amount of work.

pub(crate) mod config;
pub(crate) mod coverage;
pub(crate) mod fetch;
pub(crate) mod stream;
pub(crate) mod sync;

use std::sync::Arc;

use futures::Stream;
use tokio::sync::{watch, Semaphore};
use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockService, ChainHeadSnapshot, ChainHeadTransactionService,
};
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader, ChainStoreService,
    CompactBlockRead as StoreCompactBlockRead, SpentOutputIndex, StoredBlockRead, TransactionIndex,
    TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_primitives::types::{
    rpc::ChainTip, AbsoluteChainWork, AddressBalance, AddressDelta, BlockHash, BlockHeader,
    BlockRef, ChainStateEpoch, CompactBlock, Height, Outpoint, ShieldedPool, SubtreeIndex,
    SubtreeRoot, TransactionId, TransparentAddress, Treestate, Utxo,
};

use crate::block::ChainBlock;
use crate::capability::{
    Answerable, ChainCapability, ServedCapabilities, ServiceabilityManifest, ServiceableRange,
};
use crate::error::{ChainViewError, Result};
use crate::ports::{
    AddressRead, BlockRead, ChainView, ChainViewSnapshot, CompactBlockRead, ForkReconcile,
    SpendRead, TransactionRead, TreestateRead, TxOutSetRead,
};
use crate::source::ChainViewSource;
use crate::types::{
    BlockId, ChainScope, ChainTxPosition, Locator, RawTransaction, SpendStatus,
    TransactionLocations,
};

use coverage::{Coverage, Provider};

pub use config::ChainViewConfig;
pub use sync::ChainViewSync;

/// A chain view over a store, a chain head, and a validator.
#[derive(Debug, Clone)]
pub struct ChainViewComposer<Store, Head, Source> {
    store: Store,
    head: Head,
    source: Arc<Source>,
    config: Arc<ChainViewConfig>,
    /// What this deployment has chosen to offer.
    ///
    /// Read by the manifest and by the reads alike, which is what stops
    /// "advertise" and "answer" drifting apart.
    served: ServedCapabilities,
    /// Bounds validator fetches across every client sharing this view.
    ///
    /// On the composer, not the snapshot: one per snapshot would be one per
    /// client, which is exactly the unbounded case this prevents.
    permits: Arc<Semaphore>,
}

impl<Store, Head, Source> ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    /// A chain view over these three providers, offering everything they can
    /// answer.
    ///
    /// The right constructor for a deployment with nothing to withhold, which
    /// is every deployment in this workspace. Offering everything is not a
    /// claim that everything is answerable: the manifest still reports a
    /// capability the tiers cannot supply as
    /// [`Absent`](crate::Answerable::Absent), because it consults coverage and
    /// the store's index set as well.
    ///
    /// Use [`builder`](Self::builder) to offer less than the tiers can do.
    pub fn new(store: Store, head: Head, source: Arc<Source>, config: ChainViewConfig) -> Self {
        Self::assembled(store, head, source, config, ServedCapabilities::ALL)
    }

    /// A chain view offering only the capabilities named on the way out.
    ///
    /// Starts at [`ServedCapabilities::CORE`] — the capabilities every chain
    /// view answers — and takes the three optional ones by name. See
    /// [`ChainViewComposerBuilder`].
    pub fn builder(
        store: Store,
        head: Head,
        source: Arc<Source>,
    ) -> ChainViewComposerBuilder<Store, Head, Source> {
        ChainViewComposerBuilder {
            store,
            head,
            source,
            config: ChainViewConfig::default(),
            served: ServedCapabilities::CORE,
        }
    }

    /// The one assembly path, shared by both constructors.
    fn assembled(
        store: Store,
        head: Head,
        source: Arc<Source>,
        config: ChainViewConfig,
        served: ServedCapabilities,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.passthrough_permits));
        Self {
            store,
            head,
            source,
            config: Arc::new(config),
            served,
            permits,
        }
    }

    /// Captures the view every read is answered from.
    ///
    /// # Captured together, and only once
    ///
    /// The watermark and the chain-head snapshot are read here in one call.
    /// That is the coherence mechanism: read apart, the watermark could claim
    /// the store covers a height the pinned head still thinks is recent, and a
    /// read there would be answered by a store holding it under a *different
    /// hash* than the caller's view believes.
    ///
    /// Coverage is derived here too, so provider selection is later just
    /// arithmetic on two `Option<Height>`s. Reading the chain head's floor
    /// walks a boxed iterator, so doing it per read would allocate once per
    /// decision per client.
    pub fn snapshot(&self) -> ComposerSnapshot<Store::Reader, Head::Snapshot, Source> {
        let reader = self.store.reader();
        let head = self.head.current();

        let store_top = self
            .config
            .store_enabled
            .then(|| reader.watermark().tip.map(|tip| tip.height))
            .flatten();
        let head_range = head
            .best_chain()
            .next()
            .map(|floor| (floor.height(), head.best_tip().height));
        let work_anchor = head.work_anchor();

        ComposerSnapshot {
            reader,
            head,
            coverage: Coverage {
                store_top,
                head: head_range,
                work_anchor,
                source_fills: self.config.passthrough_enabled,
            },
            served: self.served,
            chainwork_offset: Arc::new(tokio::sync::OnceCell::new()),
            fetch: fetch::Fetcher::new(
                Arc::clone(&self.source),
                Arc::clone(&self.permits),
                Arc::clone(&self.config),
            ),
            config: Arc::clone(&self.config),
        }
    }

    /// What this view offers, and how far.
    ///
    /// Derived by walking [`ChainCapability::ALL`] against live coverage, so a
    /// capability cannot be advertised here and refused by a read.
    fn manifest(
        &self,
        store_capabilities: zaino_chain_store::StoreCapabilities,
    ) -> ServiceabilityManifest {
        let snapshot = self.snapshot();
        let coverage = snapshot.coverage;
        let config = &self.config;
        let served = self.served;

        ServiceabilityManifest::derive(|capability| {
            // Withheld by this deployment. Checked before anything about the
            // providers, because a capability nobody offers has no height to
            // report and the reads refuse it for the same reason.
            if !served.contains(capability) {
                return Answerable::Absent;
            }
            serviceability(capability, coverage, store_capabilities, config)
        })
    }
}

/// How far one capability is answerable.
///
/// The policy table, kept readable *as a table*: this is what a reviewer checks
/// when asking why a deployment does not advertise something.
fn serviceability(
    capability: ChainCapability,
    coverage: Coverage,
    store_capabilities: zaino_chain_store::StoreCapabilities,
    config: &ChainViewConfig,
) -> Answerable {
    use zaino_chain_store::StoreCapability;

    let Some(tip) = coverage.chain_tip() else {
        return Answerable::NotAnswerable;
    };
    let has = |capability: StoreCapability| {
        config.store_enabled && store_capabilities.contains(capability)
    };
    let passthrough = config.passthrough_enabled;

    // The contiguous ceiling: everything up to here is answerable from the
    // local providers alone. Below a hole that is the store's top; with no hole
    // it is the tip.
    let local_ceiling = match coverage.gap_from() {
        None => Some(tip),
        Some(gap) => gap.checked_sub(1),
    };

    match capability {
        // Answered from whichever provider covers the height, and the validator
        // can stand in for either — so a hole does not cap these.
        ChainCapability::Blocks | ChainCapability::CompactBlocks => {
            let index = if capability == ChainCapability::Blocks {
                StoreCapability::StoredBlocks
            } else {
                StoreCapability::CompactBlocks
            };
            if !has(index) && !passthrough {
                return Answerable::Absent;
            }
            if passthrough {
                Answerable::ToHeight(tip)
            } else {
                local_ceiling.map_or(Answerable::NotAnswerable, Answerable::ToHeight)
            }
        }

        // No provider but the validator holds consensus bytes or serialized
        // commitment trees.
        ChainCapability::Transactions
        | ChainCapability::Treestate
        | ChainCapability::SubtreeRoots => {
            if passthrough {
                Answerable::ToHeight(tip)
            } else {
                Answerable::Absent
            }
        }

        // The recent window's own graph; independent of what lies below it.
        ChainCapability::ChainTips => Answerable::ToHeight(tip),

        // Zaino's own indexes. The validator runs none of these, so a hole caps
        // them at the contiguous ceiling rather than being filled — and without
        // the index they are absent outright.
        ChainCapability::SpendStatus => {
            if !has(StoreCapability::SpentOutputs) {
                return Answerable::Absent;
            }
            local_ceiling.map_or(Answerable::NotAnswerable, Answerable::ToHeight)
        }
        ChainCapability::TxOutSet => {
            if !has(StoreCapability::TxOutSet) {
                return Answerable::Absent;
            }
            coverage
                .store_top
                .map_or(Answerable::NotAnswerable, Answerable::ToHeight)
        }
        // Zaino builds no transparent address index yet, so this is the
        // validator's answer for now. When the index lands it joins the group
        // above and a hole starts capping it.
        ChainCapability::AddressHistory => {
            if passthrough {
                Answerable::ToHeight(tip)
            } else {
                Answerable::Absent
            }
        }
    }
}

impl<Store, Head, Source> ChainView for ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService<Reader: StoredBlockRead + StoreCompactBlockRead + TransactionIndex>,
    Head: ChainHeadBlockService<Snapshot: ChainHeadTransactionService>,
    Source: ChainViewSource,
{
    type Snapshot = ComposerSnapshot<Store::Reader, Head::Snapshot, Source>;

    fn snapshot(&self) -> Self::Snapshot {
        ChainViewComposer::snapshot(self)
    }

    fn subscribe_tip(&self) -> watch::Receiver<ChainStateEpoch> {
        self.head.subscribe_updates()
    }

    fn serviceability(&self) -> ServiceabilityManifest {
        self.manifest(self.store.reader().capabilities())
    }
}

/// Assembles a chain view that offers less than its tiers could.
///
/// Starts at [`ServedCapabilities::CORE`] and takes the three optional
/// capabilities by name.
///
/// # The methods are type-gated
///
/// Each `serving_*` method exists **only** when both tiers can supply its half.
/// `serving_spend_status` requires the store's reader to implement
/// [`SpentOutputIndex`] and the head's snapshot to implement
/// [`ChainHeadTransactionService`]; a composition missing either does not have
/// the method, so the advertisement cannot be made.
///
/// That is the property worth having. A runtime check would let a deployment
/// advertise a capability, fail every read of it, and look like a fault rather
/// than a misconfiguration — and it would be discovered by a client, in
/// production, rather than by the compiler.
///
/// It is a floor, not a ceiling. Naming a capability offers it; whether it is
/// *answerable*, and to what height, is still decided by the manifest from live
/// coverage and the store's runtime index set. A deployment can offer spend
/// status and have the manifest report `NotAnswerable` while the store is still
/// building.
pub struct ChainViewComposerBuilder<Store, Head, Source> {
    store: Store,
    head: Head,
    source: Arc<Source>,
    config: ChainViewConfig,
    served: ServedCapabilities,
}

impl<Store, Head, Source> ChainViewComposerBuilder<Store, Head, Source>
where
    Store: ChainStoreService,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    /// Uses this configuration rather than the default.
    pub fn with_config(mut self, config: ChainViewConfig) -> Self {
        self.config = config;
        self
    }

    /// Offers spend status: whether an outpoint is spent, and by what.
    ///
    /// A merge, so both tiers must supply their half — the store for spends
    /// below the seam, the head for spends inside the window.
    pub fn serving_spend_status(mut self) -> Self
    where
        Store::Reader: SpentOutputIndex,
        Head::Snapshot: ChainHeadTransactionService,
    {
        self.served = self.served.with(ChainCapability::SpendStatus);
        self
    }

    /// Offers the unspent transparent output set's running totals.
    ///
    /// The store's alone: the accumulator is a finalised-chain quantity and the
    /// window contributes nothing to it.
    pub fn serving_txout_set(mut self) -> Self
    where
        Store::Reader: TxOutSetIndex,
    {
        self.served = self.served.with(ChainCapability::TxOutSet);
        self
    }

    /// Offers transparent address history.
    ///
    /// A merge like spend status: the store's finalised effects joined with the
    /// window's recent ones.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub fn serving_address_history(mut self) -> Self
    where
        Store::Reader: zaino_chain_store::TransparentHistoryIndex,
        Head::Snapshot: zaino_chain_head::ChainHeadTransparentHistoryService,
    {
        self.served = self.served.with(ChainCapability::AddressHistory);
        self
    }

    /// The chain view.
    pub fn build(self) -> ChainViewComposer<Store, Head, Source> {
        ChainViewComposer::assembled(self.store, self.head, self.source, self.config, self.served)
    }
}

/// A chain view pinned to one tip.
pub struct ComposerSnapshot<Reader, HeadSnapshot, Source> {
    reader: Reader,
    head: Arc<HeadSnapshot>,
    /// What each provider covers, captured with the tiers themselves.
    coverage: Coverage,
    /// What this deployment offers. The same set the manifest reports from, so
    /// a read cannot answer something the manifest called absent.
    served: ServedCapabilities,
    /// The anchor's absolute chainwork, resolved at most once.
    ///
    /// Shared across clones rather than copied: a clone is the same pinned
    /// view, so it is the same answer, and resolving it costs a store read.
    /// The inner `None` — the store has not built as far as the anchor — is
    /// cached like any other answer, because coverage is pinned too and so
    /// cannot become resolvable within one snapshot.
    chainwork_offset: Arc<tokio::sync::OnceCell<AnchorChainWork>>,
    fetch: fetch::Fetcher<Source>,
    config: Arc<ChainViewConfig>,
}

/// Cloning shares the providers rather than duplicating them.
///
/// Hand-written: the derive would demand `HeadSnapshot: Clone` and
/// `Source: Clone`, which is what the `Arc`s exist to avoid. Cheap enough that
/// a stream can own a clone, which is what makes a returned stream `'static`.
impl<Reader: Clone, HeadSnapshot, Source> Clone for ComposerSnapshot<Reader, HeadSnapshot, Source> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            head: Arc::clone(&self.head),
            coverage: self.coverage,
            served: self.served,
            chainwork_offset: Arc::clone(&self.chainwork_offset),
            fetch: self.fetch.clone(),
            config: Arc::clone(&self.config),
        }
    }
}

impl<Reader, HeadSnapshot, Source> core::fmt::Debug
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComposerSnapshot")
            .field("coverage", &self.coverage)
            .finish_non_exhaustive()
    }
}

/// A store failure, classified for a consumer.
pub(crate) fn store_err(error: ChainStoreError) -> ChainViewError {
    match error {
        ChainStoreError::NotReady => {
            ChainViewError::Transient(String::from("finalised store is not ready"))
        }
        ChainStoreError::Unavailable(index) => ChainViewError::NotServiceable(match index {
            zaino_chain_store::StoreCapability::CompactBlocks => {
                "the store does not build the compact-block index"
            }
            zaino_chain_store::StoreCapability::SpentOutputs => {
                "the store does not build the spent-output index"
            }
            zaino_chain_store::StoreCapability::TxOutSet => {
                "the store does not build the txout-set accumulator"
            }
            _ => "the store does not build a required index",
        }),
        // Coverage decides which provider answers *before* asking, so either of
        // these means it chose wrongly — a fault here, not a condition there.
        // Reporting them as `NotServiceable` would make a routing bug look like
        // a store that has not caught up, which happens often enough that
        // nobody would look twice.
        other @ (ChainStoreError::AboveWatermark { .. } | ChainStoreError::InvalidRange { .. }) => {
            ChainViewError::Fatal(format!("chain view routed a read wrongly: {other}"))
        }
        other => ChainViewError::Fatal(other.to_string()),
    }
}

/// A chain-head block's work made absolute, given the anchor's chainwork.
///
/// `None` when the anchor's chainwork is unknown, and also on overflow — which
/// no real chain reaches, and which is reported absent rather than wrong for
/// the same reason every other unknown is.
fn rebase(anchor: AnchorChainWork, block: &ChainHeadBlock) -> Option<AbsoluteChainWork> {
    let relative = u128::from(block.work);
    let absolute = match anchor {
        AnchorChainWork::Known(anchor) => core::num::NonZeroU128::from(anchor)
            .get()
            .checked_add(relative)?,
        AnchorChainWork::Unknown => return None,
    };
    core::num::NonZeroU128::new(absolute).map(AbsoluteChainWork::new)
}

/// The absolute chainwork of the chain head's anchor.
#[derive(Debug, Clone, Copy)]
enum AnchorChainWork {
    /// The anchor's absolute chainwork, read from the store.
    Known(AbsoluteChainWork),
    /// The store has not built up to the anchor yet.
    Unknown,
}

/// This deployment does not offer `capability`.
///
/// Distinct from the tiers being unable to answer: they may well be able, and
/// the deployment has chosen not to. The message says so, because "not
/// serviceable" reads as a fault otherwise and an operator needs to know which
/// of the two to go and change.
///
/// The manifest reports the same capabilities as
/// [`Absent`](crate::Answerable::Absent) from the same set, so a consumer that
/// checked first never reaches this.
fn withheld(capability: ChainCapability) -> ChainViewError {
    ChainViewError::NotServiceable(match capability {
        ChainCapability::Blocks => "this deployment does not serve blocks",
        ChainCapability::CompactBlocks => "this deployment does not serve compact blocks",
        ChainCapability::Transactions => "this deployment does not serve transactions",
        ChainCapability::Treestate => "this deployment does not serve treestates",
        ChainCapability::SubtreeRoots => "this deployment does not serve subtree roots",
        ChainCapability::ChainTips => "this deployment does not serve chain tips",
        ChainCapability::AddressHistory => {
            "this deployment does not serve transparent address history"
        }
        ChainCapability::SpendStatus => "this deployment does not serve spend status",
        ChainCapability::TxOutSet => "this deployment does not serve the txout set",
    })
}

/// The chain view cannot cover a height from any provider.
fn uncoverable() -> ChainViewError {
    ChainViewError::NotServiceable("no provider covers this height and the validator is disabled")
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    HeadSnapshot: ChainHeadSnapshot,
{
    /// The block the chain head holds at `height`, if it holds one.
    fn head_block(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.head.best_block_by_height(height)
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoredBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
{
    /// A chain-head block's work, made absolute.
    ///
    /// The chain head measures work from its own anchor — the parent of the
    /// window floor — which contributes none, so this is one addition:
    ///
    /// ```text
    /// absolute(B) = chainwork(anchor) + work(B)
    /// ```
    ///
    /// `None` when the anchor's chainwork is not known, which is the honest
    /// answer rather than a placeholder: see [`ChainBlock::chainwork`].
    ///
    /// Correct for a competing block as well as a canonical one. Every block
    /// the head retains accumulates from the same anchor, and a competing
    /// block's chainwork is a real quantity — it is what makes one branch
    /// heavier than another.
    async fn absolute_chainwork(
        &self,
        block: &ChainHeadBlock,
    ) -> Result<Option<AbsoluteChainWork>> {
        Ok(rebase(self.chainwork_offset().await?, block))
    }

    /// The absolute chainwork of the chain head's anchor.
    ///
    /// Read from the store, because the store is the only provider whose
    /// coverage reaches genesis and so the only one that can know a cumulative
    /// quantity. `None` while it has not built that far: there is no correct
    /// value to serve until then.
    ///
    /// Resolved at most once per snapshot. The whole block is read for one
    /// number, which no store port offers alone — acceptable exactly because it
    /// happens once and is then shared by every read and every clone.
    ///
    /// Looking the anchor up by height is sound: the store is append-only, so
    /// it never holds a different block at a height it has already built.
    /// `rewind_to` is a repair path with no caller outside the store.
    async fn chainwork_offset(&self) -> Result<AnchorChainWork> {
        self.chainwork_offset
            .get_or_try_init(|| async {
                let anchor = self.coverage.work_anchor;

                if self
                    .coverage
                    .store_top
                    .is_none_or(|top| top < anchor.height)
                {
                    return Ok(AnchorChainWork::Unknown);
                }

                Ok(self
                    .reader
                    .blocks_chunk(anchor.height, anchor.height)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .next()
                    .map_or(AnchorChainWork::Unknown, |block| {
                        AnchorChainWork::Known(block.chainwork)
                    }))
            })
            .await
            .copied()
    }
}

impl<Reader, HeadSnapshot, Source> stream::Coverable
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
{
    fn coverage(&self) -> Coverage {
        self.coverage
    }

    fn chunk_budget_bytes(&self) -> usize {
        self.config.chunk_budget_bytes
    }
}

// ***** Chain shape *****

impl<Reader, HeadSnapshot, Source> ChainViewSnapshot
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoredBlockRead + StoreCompactBlockRead + TransactionIndex,
    HeadSnapshot: ChainHeadSnapshot + ChainHeadTransactionService,
    Source: ChainViewSource,
{
    fn tip(&self) -> BlockRef {
        self.head.best_tip()
    }

    fn serviceable_range(&self) -> ServiceableRange {
        ServiceableRange {
            finalised_tip: self.coverage.store_top,
            tip: self.coverage.chain_tip(),
            gap_from: self.coverage.gap_from(),
        }
    }
}

impl<Reader, HeadSnapshot, Source> ForkReconcile for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoredBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    fn chain_tips(&self) -> Vec<ChainTip> {
        self.head.chain_tips()
    }

    async fn fork_point(&self, locator: &Locator) -> Result<Option<BlockRef>> {
        // The locator is most-recent-first, so the first hash still on the
        // canonical chain is the newest common ancestor — the point a client
        // resumes from.
        for hash in locator.hashes() {
            if let Some(block) = self.head.block_by_hash(hash) {
                if self.head.is_on_best_chain(block.reference) {
                    return Ok(Some(block.reference));
                }
                // Retained, but on a competing branch: the client is on a fork
                // this view rejected, so keep looking further back.
                continue;
            }
            if self.coverage.store_top.is_some() {
                if let Some(height) = self.reader.block_height(*hash).await.map_err(store_err)? {
                    return Ok(Some(BlockRef {
                        hash: *hash,
                        height,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn stream_blocks_to_tip(
        &self,
        from: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        let end = self.coverage.chain_tip().unwrap_or(Height::GENESIS);
        self.stream_blocks(from, end)
    }
}

// ***** Blocks *****

impl<Reader, HeadSnapshot, Source> BlockRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoredBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>> {
        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(Provider::Store) => self.reader.block_hash(height).await.map_err(store_err),
            Some(Provider::Head) => Ok(self.head_block(height).map(ChainHeadBlock::hash)),
            Some(Provider::Source) => Ok(self
                .fetch
                .block_at(height)
                .await?
                .map(|(block, _)| block.header.hash)),
        }
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>> {
        // Id-addressed: there is no height to select a provider with, so ask
        // the providers that index by hash and then the validator.
        //
        // The chain head first — an in-memory lookup where the store's is a
        // disk read, and the only provider retaining competing branches, so the
        // only one that can tell a canonical block from an orphaned one.
        if let Some(block) = self.head.block_by_hash(&hash) {
            return Ok(self
                .head
                .is_on_best_chain(block.reference)
                .then(|| block.height()));
        }

        if self.coverage.store_top.is_some() {
            if let Some(height) = self.reader.block_height(hash).await.map_err(store_err)? {
                return Ok(Some(height));
            }
        }

        // Neither holds it. During catch-up that is the ordinary case for any
        // historical hash, so ask the validator rather than reporting a block
        // that plainly exists as absent.
        if !self.fetch.enabled() {
            return Ok(None);
        }
        Ok(self
            .fetch
            .block_by_hash(hash)
            .await?
            .map(|block| block.header.height))
    }

    async fn block(&self, at: BlockId) -> Result<Option<ChainBlock>> {
        let height = match at {
            BlockId::Height(height) => height,
            BlockId::Hash(hash) => {
                // The chain head can answer for a competing block, which no
                // height names — so try it before resolving.
                if let Some(block) = self.head.block_by_hash(&hash) {
                    let chainwork = self.absolute_chainwork(block).await?;
                    return Ok(Some(ChainBlock::from_parsed(
                        &block.block,
                        block.tree_roots.clone(),
                        chainwork,
                    )));
                }
                match self.block_height(hash).await? {
                    Some(height) => height,
                    None => return Ok(None),
                }
            }
        };

        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(Provider::Store) => Ok(self
                .reader
                .blocks_chunk(height, height)
                .await
                .map_err(store_err)?
                .into_iter()
                .next()
                .map(ChainBlock::from_stored)),
            Some(Provider::Head) => match self.head_block(height) {
                None => Ok(None),
                Some(block) => {
                    let chainwork = self.absolute_chainwork(block).await?;
                    Ok(Some(ChainBlock::from_parsed(
                        &block.block,
                        block.tree_roots.clone(),
                        chainwork,
                    )))
                }
            },
            // The validator reports no work through this path, so there is
            // nothing to rebase and nothing to serve.
            Some(Provider::Source) => Ok(self
                .fetch
                .block_at(height)
                .await?
                .map(|(block, roots)| ChainBlock::from_parsed(&block, roots, None))),
        }
    }

    async fn block_header(&self, at: BlockId) -> Result<Option<BlockHeader>> {
        Ok(self.block(at).await?.map(|block| block.header))
    }

    async fn raw_block(&self, at: BlockId) -> Result<Option<Vec<u8>>> {
        // Consensus bytes: no provider retains them, so this is the validator's
        // answer wherever the block sits — asked the way the caller asked, so a
        // by-height request costs one round trip rather than a resolution and a
        // fetch, and still works for a height no provider covers.
        self.fetch
            .require("raw blocks need the validator, which is disabled")?;
        match at {
            BlockId::Height(height) => self.fetch.raw_block_at(height).await,
            BlockId::Hash(hash) => {
                fetch::miss(self.fetch.source().get_raw_block_by_hash(hash).await)
            }
        }
    }

    fn stream_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        stream::walk(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(async move { snapshot.blocks_from(segment.provider, heights).await })
            },
        )
    }

    fn stream_raw_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<Vec<u8>>>> + Send + use<Reader, HeadSnapshot, Source> {
        // Always the validator, so the plan is only used to bound the range at
        // the chain tip — no provider holds consensus bytes.
        stream::walk(
            self.clone(),
            start,
            end,
            move |snapshot, _segment, heights| {
                Box::pin(async move {
                    snapshot
                        .fetch
                        .require("raw blocks need the validator, which is disabled")?;
                    snapshot.fetch.fill_raw_blocks(heights).await
                })
            },
        )
    }
}

// ***** Per-provider batch reads, shared by the point and range paths *****

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoredBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    /// Indexed blocks for a run of heights, from one provider.
    async fn blocks_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
    ) -> Result<Vec<ChainBlock>> {
        let (Some(first), Some(last)) = (heights.first().copied(), heights.last().copied()) else {
            return Ok(Vec::new());
        };
        match provider {
            Provider::Store => Ok(self
                .reader
                .blocks_chunk(first, last)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(ChainBlock::from_stored)
                .collect()),
            Provider::Head => {
                // Resolved once for the run rather than per block: it is the
                // same anchor for every block in the window.
                let offset = self.chainwork_offset().await?;
                Ok(self
                    .head
                    .best_chain_blocks(first, last)
                    .map_err(|error| ChainViewError::Fatal(error.to_string()))?
                    .map(|block| {
                        ChainBlock::from_parsed(
                            &block.block,
                            block.tree_roots.clone(),
                            rebase(offset, block),
                        )
                    })
                    .collect())
            }
            // The validator reports no work through this path, so there is
            // nothing to rebase and nothing to serve.
            Provider::Source => Ok(self
                .fetch
                .fill_blocks(heights)
                .await?
                .into_iter()
                .map(|(block, roots)| ChainBlock::from_parsed(&block, roots, None))
                .collect()),
        }
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    /// Compact blocks for a run of heights, from one provider.
    async fn compact_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<CompactBlock>> {
        let (Some(first), Some(last)) = (heights.first().copied(), heights.last().copied()) else {
            return Ok(Vec::new());
        };
        match provider {
            // The filter is pushed into the store's read, where it decides
            // which row families decode at all.
            Provider::Store => self
                .reader
                .compact_chunk(first, last, pools)
                .await
                .map_err(store_err),
            Provider::Head => Ok(self
                .head
                .best_chain_blocks(first, last)
                .map_err(|error| ChainViewError::Fatal(error.to_string()))?
                .map(|block| {
                    stream::compact_from_pre_index(
                        zaino_primitives::types::PreIndexCompactBlock::from(&block.block),
                        stream::metadata_from_roots(&block.tree_roots),
                        pools,
                    )
                })
                .collect()),
            Provider::Source => self.fetch.fill_compact(heights, pools).await,
        }
    }
}

// ***** Compact blocks *****

impl<Reader, HeadSnapshot, Source> CompactBlockRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn compact_block(
        &self,
        height: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Option<CompactBlock>> {
        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(provider) => Ok(self
                .compact_from(provider, vec![height], pools)
                .await?
                .into_iter()
                .next()),
        }
    }

    fn stream_compact(
        &self,
        start: Height,
        end: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> impl Stream<Item = Result<Vec<CompactBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        stream::walk_sized(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(async move {
                    snapshot
                        .compact_from(segment.provider, heights, pools)
                        .await
                })
            },
            |blocks| blocks.iter().map(stream::approx_size).sum(),
        )
    }
}

// ***** Transactions *****

impl<Reader, HeadSnapshot, Source> TransactionRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TransactionIndex,
    HeadSnapshot: ChainHeadSnapshot + ChainHeadTransactionService,
    Source: ChainViewSource,
{
    async fn raw_transaction(&self, txid: TransactionId) -> Result<Option<RawTransaction>> {
        self.fetch
            .require("raw transactions need the validator, which is disabled")?;
        Ok(
            fetch::miss(self.fetch.source().get_transaction(txid).await)?.map(|response| {
                RawTransaction {
                    bytes: response.bytes,
                    location: response.location,
                }
            }),
        )
    }

    async fn transaction_locations(&self, txid: TransactionId) -> Result<TransactionLocations> {
        // The chain head answers first and answers most: it is the only
        // provider retaining competing branches, so the only source of the
        // non-best half.
        let recent = self.head.transaction_locations(&txid);
        let mut locations = TransactionLocations {
            best_chain: recent.best_chain.map(|at| ChainTxPosition {
                block: at.block,
                tx_index: at.tx_index,
            }),
            non_best_chain: recent
                .non_best_chain
                .into_iter()
                .map(|at| ChainTxPosition {
                    block: at.block,
                    tx_index: at.tx_index,
                })
                .collect(),
        };

        if locations.best_chain.is_some() || self.coverage.store_top.is_none() {
            return Ok(locations);
        }

        // Below the window there is a single chain, so the store's
        // best-chain-only index is the whole answer for its range.
        let Some(position) = self.reader.tx_position(&txid).await.map_err(store_err)? else {
            return Ok(locations);
        };
        // The store answers with a height; a position names a block, so
        // complete it with the hash at that height.
        if let Some(hash) = self
            .reader
            .block_hash(position.height)
            .await
            .map_err(store_err)?
        {
            locations.best_chain = Some(ChainTxPosition {
                block: BlockRef {
                    hash,
                    height: position.height,
                },
                tx_index: position.tx_index,
            });
        }
        Ok(locations)
    }
}

// ***** Commitment trees *****

impl<Reader, HeadSnapshot, Source> TreestateRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn treestate(&self, at: BlockId) -> Result<Option<Treestate>> {
        // A treestate carries the *serialized* commitment tree; both providers
        // keep only a root and a size per pool, and a root is not a tree. So
        // this is the validator's answer, asked the way the caller asked it —
        // `z_gettreestate` takes either.
        self.fetch
            .require("treestates need the validator, which is disabled")?;
        match at {
            BlockId::Height(height) => fetch::miss(self.fetch.source().get_treestate(height).await),
            BlockId::Hash(hash) => {
                fetch::miss(self.fetch.source().get_treestate_by_hash(hash).await)
            }
        }
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>> {
        // Indexed by subtree completion rather than by height, and no provider
        // keeps that index — deriving the boundaries from per-block roots means
        // walking the chain from genesis.
        self.fetch
            .require("subtree roots need the validator, which is disabled")?;
        Ok(fetch::miss(
            self.fetch
                .source()
                .get_subtree_roots(pool, start_index, limit)
                .await,
        )?
        .unwrap_or_default())
    }
}

// ***** Optional capabilities *****
//
// Each is implemented only where its providers can supply it, so a deployment
// whose store lacks the backing index gets a view that genuinely does not have
// the trait — checked by the compiler rather than reported at runtime.

impl<Reader, HeadSnapshot, Source> SpendRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + SpentOutputIndex,
    HeadSnapshot: ChainHeadSnapshot + ChainHeadTransactionService,
    Source: ChainViewSource,
{
    async fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
        scope: ChainScope,
    ) -> Result<Vec<SpendStatus>> {
        if !self.served.contains(ChainCapability::SpendStatus) {
            return Err(withheld(ChainCapability::SpendStatus));
        }
        if self.coverage.store_top.is_none() {
            return Err(ChainViewError::NotServiceable(
                "spend status needs the finalised store, which is disabled",
            ));
        }

        let finalised = self
            .reader
            .outpoint_spenders(outpoints)
            .await
            .map_err(store_err)?;

        if scope == ChainScope::Finalised {
            return Ok(finalised
                .into_iter()
                .map(|spender| match spender {
                    Some(spender) => SpendStatus::SpentBy(spender.txid),
                    None => SpendStatus::Unspent,
                })
                .collect());
        }

        // The validator runs no spend index, so a hole between the providers
        // cannot be filled and a full-chain answer spanning one would be
        // silently incomplete. Unlike a block read there is nothing to degrade
        // to, so refusing is the only honest option.
        if !self.coverage.contiguous() {
            return Err(ChainViewError::NotServiceable(
                "spend status cannot span the range the finalised store has not built",
            ));
        }

        // The recent window wins where it has an answer: an outpoint is spent
        // at most once on the best chain, so the two cannot disagree — but they
        // can be at different points in time, and the window is the later one.
        Ok(self
            .head
            .outpoint_spenders(outpoints)
            .into_iter()
            .zip(finalised)
            .map(|(recent, finalised)| match (recent, finalised) {
                (Some(spender), _) => SpendStatus::SpentBy(spender.txid),
                (None, Some(spender)) => SpendStatus::SpentBy(spender.txid),
                (None, None) => SpendStatus::Unspent,
            })
            .collect())
    }
}

impl<Reader, HeadSnapshot, Source> TxOutSetRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TxOutSetIndex,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn txout_set(&self) -> Result<TxOutSetAccumulator> {
        if !self.served.contains(ChainCapability::TxOutSet) {
            return Err(withheld(ChainCapability::TxOutSet));
        }
        if self.coverage.store_top.is_none() {
            return Err(ChainViewError::NotServiceable(
                "the txout set needs the finalised store, which is disabled",
            ));
        }
        self.reader.txout_set().await.map_err(store_err)
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Source: ChainViewSource,
{
    /// The shape every transparent-address read shares.
    ///
    /// Each one refuses the same two ways before it asks anything — the
    /// deployment may withhold the capability, and the validator may be
    /// disabled — and each treats "the validator does not know this address" as
    /// an absent answer rather than a failure. Only the port call differs, so
    /// it arrives as the future it returns and the rest is written once.
    ///
    /// A caller builds that future before the guards run, which costs nothing:
    /// an `async fn` does no work until awaited, so a withheld capability still
    /// never reaches the validator.
    ///
    /// Returns `Option` rather than defaulting here because the callers do not
    /// agree on what absence means: a balance is zero, a list is empty.
    async fn address_query<T, E>(
        &self,
        call: impl core::future::Future<Output = core::result::Result<T, zaino_source::QueryError<E>>>,
    ) -> Result<Option<T>>
    where
        E: core::fmt::Debug + core::fmt::Display,
    {
        if !self.served.contains(ChainCapability::AddressHistory) {
            return Err(withheld(ChainCapability::AddressHistory));
        }
        self.fetch
            .require("address history needs the validator, which is disabled")?;
        fetch::miss(call.await)
    }
}

impl<Reader, HeadSnapshot, Source> AddressRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    // Zaino builds no transparent address index today, so these are the
    // validator's answers. When the index lands they become a merge across the
    // providers and gain a store bound, at which point a hole stops being
    // fillable — the validator runs no such index either.

    async fn address_balance(&self, addresses: &[TransparentAddress]) -> Result<AddressBalance> {
        Ok(self
            .address_query(self.fetch.source().get_address_balance(encoded(addresses)))
            .await?
            // An address the validator does not know has no balance, which is
            // an answer rather than a failure.
            .unwrap_or(AddressBalance {
                balance: zaino_primitives::types::Zatoshis::ZERO,
                received: zaino_primitives::types::ZatoshisFlowSum::from_summed(0),
            }))
    }

    async fn address_utxos(&self, addresses: &[TransparentAddress]) -> Result<Vec<Utxo>> {
        Ok(self
            .address_query(self.fetch.source().get_address_utxos(encoded(addresses)))
            .await?
            .unwrap_or_default())
    }

    async fn address_txids(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>> {
        Ok(self
            .address_query(
                self.fetch
                    .source()
                    .get_address_txids(encoded(addresses), start, end),
            )
            .await?
            .unwrap_or_default())
    }

    async fn address_deltas(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>> {
        Ok(self
            .address_query(
                self.fetch
                    .source()
                    .get_address_deltas(encoded(addresses), start, end),
            )
            .await?
            .unwrap_or_default())
    }
}

/// Addresses as the source ports name them.
///
/// The ports take encoded strings because that is what the validator's RPC
/// takes; this crate's surface takes the domain type, so the encoding happens
/// once here rather than at every call site.
fn encoded(addresses: &[TransparentAddress]) -> Vec<String> {
    addresses
        .iter()
        .map(|address| address.as_str().to_owned())
        .collect()
}
