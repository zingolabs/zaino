//! `zaino-store-service` — the concrete inner engine.
//!
//! Composes the finalised [`ChainStoreService`] and the non-finalised
//! [`ChainHeadBlockService`] into one `zaino-service` `IndexerService`. This is
//! the real backend behind the profiles the adapters consume.
//!
//! **Slice 1 (this file).** The composition's *seam* is wired against the real
//! ports — both are shared vocabulary (`Height` / `BlockHash` / `BlockId`), so
//! the pin, the serviceable range, and the tip cost no conversion:
//! [`TakeSnapshot`] captures a chain-head snapshot (the pin) alongside a store
//! reader, and the [`Snapshot`] marker reads its coordinates from them. The
//! conversion-heavy per-block reads (`StoredBlock` / `ChainHeadBlock` → domain
//! `Block` / `CompactBlock` / `Treestate` / …) return `NotServiceable` for now;
//! they land on the churning primitives and are the deferred re-seam. The
//! controls that need the mempool / validator handles are likewise stubbed.
//!
//! The milestone this slice proves is the static assertion in `tests`: the
//! composed engine type-checks as **every** profile (`WalletLibService`,
//! `LightServeService`, `NodeRpcService`) over the real ports.
#![forbid(unsafe_code)]

use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chain_head::ports::ChainHeadBlockService;
use zaino_chain_head::snapshot::ChainHeadSnapshot;
use zaino_chain_store::ports::{ChainStoreReader, ChainStoreService};

use zaino_core::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockId, BlockRef, Capability,
    ChainInfo, CompactBlock, Height, HeightRange, MempoolTx, Outpoint, PassthroughAnswer,
    PassthroughQuery, ReportedUpgrade, ServiceabilityManifest, ServiceableRange, ShieldedPool,
    SpendStatus, SubtreeRoot, TipEvent, Transaction, TransactionId, TransparentAddress, Treestate,
    TxStatus, Utxo,
};
use zaino_service::error::{
    AddressReadError, BlockReadError, BroadcastRejection, ReadError, SpendReadError, Transient,
    TreestateReadError, TxReadError,
};
use zaino_service::{
    AddressRead, BlockRead, Broadcast, ChainInfoRead, CompactBlockRead, CompactNullifierRead,
    IndexerService, MempoolSubscribe, Passthrough, ReportedUpgrades, Serviceable, Snapshot,
    SpendRead, TakeSnapshot, TipSubscribe, TransactionRead, TreestateRead,
};

/// Genesis height, for an empty finalised store.
fn genesis() -> Height {
    Height::try_from(0).expect("0 is a valid height")
}

/// The concrete engine: the finalised store `S` and the non-finalised chain head
/// `H`, composed.
#[derive(Clone)]
pub struct Engine<S, H> {
    store: S,
    head: H,
}

impl<S, H> Engine<S, H>
where
    S: ChainStoreService,
    H: ChainHeadBlockService,
{
    /// Compose a store service and a chain-head service into one engine.
    pub fn new(store: S, head: H) -> Self {
        Self { store, head }
    }
}

/// A pinned, reorg-coherent view: a store reader for the finalised range and a
/// captured chain-head snapshot for the non-finalised tip. Routing between them
/// is by the store watermark (slice 2).
pub struct StoreSnapshot<R, N> {
    reader: R,
    head: Arc<N>,
}

impl<R: Clone, N> Clone for StoreSnapshot<R, N> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            head: Arc::clone(&self.head),
        }
    }
}

// --- controls (on the engine) ------------------------------------------------

impl<S, H> TakeSnapshot for Engine<S, H>
where
    S: ChainStoreService,
    H: ChainHeadBlockService,
{
    type Snapshot = StoreSnapshot<S::Reader, H::Snapshot>;

    async fn snapshot(&self) -> Result<Self::Snapshot, Transient> {
        // Capture the current chain-head view (the pin) and a store reader once,
        // so every read through the returned snapshot is coherent.
        Ok(StoreSnapshot {
            reader: self.store.reader(),
            head: self.head.current(),
        })
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> TipSubscribe for Engine<S, H> {
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent> {
        // Slice 4: bridge the chain-head epoch watch to tip events.
        stream::empty().boxed()
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> MempoolSubscribe for Engine<S, H> {
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        // Slice 4: wire the mempool handle.
        stream::empty().boxed()
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> Broadcast for Engine<S, H> {
    async fn broadcast(&self, _raw_tx: Vec<u8>) -> Result<TransactionId, BroadcastRejection> {
        // Slice 4: relay through the validator handle.
        Err(BroadcastRejection::Invalid(
            "broadcast not wired yet".into(),
        ))
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> Serviceable for Engine<S, H> {
    fn serviceability(&self) -> ServiceabilityManifest {
        // Slice 2: derive from the store capabilities + watermark.
        ServiceabilityManifest::default()
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> ReportedUpgrades for Engine<S, H> {
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, ReadError> {
        // Slice 4: pass through the validator schedule.
        Ok(Vec::new())
    }
}

impl<S: Send + Sync + 'static, H: Send + Sync + 'static> Passthrough for Engine<S, H> {
    async fn passthrough(&self, _query: PassthroughQuery) -> Result<PassthroughAnswer, Transient> {
        // Slice 4: relay to the validator.
        Err(Transient("passthrough not wired yet".into()))
    }
}

impl<S, H> IndexerService for Engine<S, H>
where
    S: ChainStoreService,
    H: ChainHeadBlockService,
{
}

// --- coherence marker (on the snapshot) --------------------------------------

impl<R, N> Snapshot for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    fn pinned_tip(&self) -> Option<BlockId> {
        // The tip this view is pinned to — the chain-head best tip. Shared
        // vocabulary, no conversion.
        let tip = self.head.best_tip();
        Some(BlockId {
            height: tip.height,
            hash: tip.hash,
        })
    }

    fn serviceable_range(&self) -> ServiceableRange {
        // Finalised seam from the store watermark; tip from the chain head.
        let finalized_tip = self
            .reader
            .watermark()
            .tip
            .map(|r| r.height)
            .unwrap_or_else(genesis);
        ServiceableRange {
            finalized_tip,
            tip: self.head.best_tip().height,
        }
    }
}

// --- reads (on the snapshot) -------------------------------------------------
//
// tip / chain_info are wired (coordinates only, no conversion). The rest are
// NotServiceable until the Stored*/ChainHead* -> domain conversion re-seam.

impl<R, N> BlockRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn tip(&self) -> Result<BlockId, BlockReadError> {
        let tip = self.head.best_tip();
        Ok(BlockId {
            height: tip.height,
            hash: tip.hash,
        })
    }
    async fn block(&self, _at: BlockRef) -> Result<Option<Block>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block_header(&self, _at: BlockRef) -> Result<Option<BlockHeader>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block_height(&self, _hash: BlockHash) -> Result<Option<Height>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    fn stream_blocks(&self, _range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>> {
        stream::empty().boxed()
    }
}

impl<R, N> CompactBlockRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn compact_block(&self, _at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    fn stream_compact(
        &self,
        _range: HeightRange,
    ) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        stream::empty().boxed()
    }
}

impl<R, N> TransactionRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn transaction(&self, _id: TransactionId) -> Result<Option<Transaction>, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::Transactions))
    }
    async fn transaction_status(&self, _id: TransactionId) -> Result<TxStatus, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::Transactions))
    }
}

impl<R, N> TreestateRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn treestate(&self, _at: Height) -> Result<Treestate, TreestateReadError> {
        Err(TreestateReadError::NotServiceable(Capability::Treestate))
    }
    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _range: HeightRange,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        Err(TreestateReadError::NotServiceable(Capability::SubtreeRoots))
    }
}

impl<R, N> AddressRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn balance(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    }
    async fn unspent_outpoints(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    }
    async fn deltas(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    }
    async fn tx_ids(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    }
}

impl<R, N> SpendRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        Err(SpendReadError::NotServiceable(Capability::SpendStatus))
    }
}

impl<R, N> CompactNullifierRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn compact_block_nullifiers(
        &self,
        _at: BlockRef,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
}

impl<R, N> ChainInfoRead for StoreSnapshot<R, N>
where
    R: ChainStoreReader,
    N: ChainHeadSnapshot,
{
    async fn chain_info(&self) -> Result<ChainInfo, ReadError> {
        // Coordinates only, no conversion.
        let tip = self.head.best_tip();
        Ok(ChainInfo {
            tip: Some(BlockId {
                height: tip.height,
                hash: tip.hash,
            }),
            estimated_height: tip.height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;
    use zaino_chain_head::ports::ChainHeadBlockService;
    use zaino_chain_store::ports::ChainStoreService;
    use zaino_service::{LightServeService, NodeRpcService, WalletLibService};

    /// Slice-1 milestone: the composed engine type-checks as every public
    /// profile, over any real store + chain-head. Compile-time only.
    fn _engine_satisfies_all_profiles<S, H>()
    where
        S: ChainStoreService,
        H: ChainHeadBlockService,
        Engine<S, H>: WalletLibService + LightServeService + NodeRpcService,
    {
    }
}
