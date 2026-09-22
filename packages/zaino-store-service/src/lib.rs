//! `zaino-store-service` — the concrete inner engine.
//!
//! Composes the finalised store and the non-finalised chain head — both named
//! only through the shared `zaino-service` ports — into one `IndexerService`,
//! the real backend behind the profiles the serve adapters consume.
//!
//! The composition itself lives in [`zaino_chainview::ChainView`]: it pairs a
//! finalised [`TakeSnapshot`] source (`Fs`) with a non-finalised one (`Nfs`),
//! both of whose snapshots are a [`ChainSegment`] (the coherence coordinate) and
//! a [`CompactBlockRead`] (compact-block serving), and captures both in one shot
//! so the seam watermark and the volatile window are coherent. [`Engine`] wraps
//! that composer and dresses it as the full inner service: it forwards the pin
//! and the compact-block reads to the composed [`ChainViewSnapshot`], and stands
//! in for the reads and controls the compact-serving slice does not yet source.
//!
//! **Serviceable slice.** Compact-block serving (`compact_block` / `stream_compact`)
//! and the coherence surface (pin, coverage, serviceable range, chain info) are
//! wired against the real composed view. Passthrough capabilities are answered
//! by [`RemoteChainView`] over the resilient source ports: `Broadcast` relays a
//! wallet's transaction, and `Treestate` reads live from the validator (zaino
//! does not index it). The remaining per-block reads (`Block` / address / spend /
//! nullifier) still return `NotServiceable`; `Passthrough` refuses and the
//! streaming controls (`TipSubscribe`, `MempoolSubscribe`) are empty streams —
//! wiring those through the same provider is the next increment.
//!
//! The classification is *which provider carries a capability*: local ones on the
//! composed [`ChainView`], passthrough ones on [`RemoteChainView`]. Consumers bind
//! the **canonical** (resilient) source traits, never the raw `OneShot*` ports —
//! those belong to the adapters and the `ValidatorClient` decorator the root
//! injects.
//!
//! The milestone this crate proves is the static assertion in `tests`: the
//! composed engine type-checks as **every** profile (`WalletLibService`,
//! `LightServeService`, `NodeRpcService`) over any two composer inputs.
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chainview::{ChainView, ChainViewSnapshot};
use zaino_source::{GetTreestate, SendRawTransaction};

mod remote;
pub use remote::RemoteChainView;

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
    AddressRead, BlockRead, Broadcast, ChainInfoRead, ChainSegment, CompactBlockRead,
    CompactNullifierRead, IndexerService, MempoolSubscribe, Passthrough, ReportedUpgrades,
    Serviceable, Snapshot, SpendRead, TakeSnapshot, TipSubscribe, TransactionRead, TreestateRead,
};

/// Genesis height, the fallback tip for a view holding nothing.
fn genesis() -> Height {
    Height::try_from(0).expect("0 is a valid height")
}

/// The concrete engine: the composed FS⊕NFS chain, dressed as the full inner
/// service.
///
/// `Fs` is the finalised store source, `Nfs` the non-finalised head source. Both
/// are captured together on each [`snapshot`](TakeSnapshot::snapshot), so a read
/// through the returned [`EngineSnapshot`] is coherent.
pub struct Engine<Fs, Nfs, Src> {
    view: ChainView<Fs, Nfs>,
    remote: RemoteChainView<Src>,
}

impl<Fs: Clone, Nfs: Clone, Src: Clone> Clone for Engine<Fs, Nfs, Src> {
    fn clone(&self) -> Self {
        Self {
            view: self.view.clone(),
            remote: self.remote.clone(),
        }
    }
}

impl<Fs, Nfs, Src> Engine<Fs, Nfs, Src>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
{
    /// Compose a finalised store source, a non-finalised head source, and the
    /// validator handle into one engine. The view answers block reads from the
    /// composed FS⊕NFS chain; `source` answers the controls the view cannot —
    /// broadcast today, mempool/tip next — through the source ports, never a
    /// concrete adapter.
    pub fn new(fs: Fs, nfs: Nfs, source: Src) -> Self {
        Self {
            view: ChainView::new(fs, nfs),
            remote: RemoteChainView::new(source),
        }
    }
}

/// A pinned, reorg-coherent view over the composed chain: the composer's
/// [`ChainViewSnapshot`], dressed as the full read surface.
///
/// The coherence marker, the served range, and the compact-block reads delegate
/// to the inner composed view; the reads the compact-serving slice does not yet
/// source are `NotServiceable` stubs on top.
pub struct EngineSnapshot<F, N, Src> {
    /// Pinned local reads — the composed FS⊕NFS view.
    local: ChainViewSnapshot<F, N>,
    /// Live passthrough reads — the validator through the source ports. Captured
    /// at snapshot time; its reads are live (not pinned), which is sound for the
    /// immutable data light clients query.
    remote: RemoteChainView<Src>,
}

impl<F: Clone, N: Clone, Src: Clone> Clone for EngineSnapshot<F, N, Src> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            remote: self.remote.clone(),
        }
    }
}

// --- controls (on the engine) ------------------------------------------------

impl<Fs, Nfs, Src> TakeSnapshot for Engine<Fs, Nfs, Src>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Src: Clone + Send + Sync + 'static,
{
    type Snapshot = EngineSnapshot<Fs::Snapshot, Nfs::Snapshot, Src>;

    async fn snapshot(&self) -> Result<Self::Snapshot, Transient> {
        // Delegate to the composer so both local sides are captured in one shot
        // — the pin stays coherent across the seam. The remote handle rides along
        // for passthrough reads (live, not pinned).
        Ok(EngineSnapshot {
            local: self.view.snapshot().await?,
            remote: self.remote.clone(),
        })
    }
}

impl<Fs: Send + Sync + 'static, Nfs: Send + Sync + 'static, Src: Send + Sync + 'static> TipSubscribe
    for Engine<Fs, Nfs, Src>
{
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent> {
        // Follow-up: bridge the chain-head epoch watch to tip events.
        stream::empty().boxed()
    }
}

impl<Fs: Send + Sync + 'static, Nfs: Send + Sync + 'static, Src: Send + Sync + 'static>
    MempoolSubscribe for Engine<Fs, Nfs, Src>
{
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        // Follow-up: wire the mempool handle.
        stream::empty().boxed()
    }
}

impl<Fs, Nfs, Src> Broadcast for Engine<Fs, Nfs, Src>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: SendRawTransaction + 'static,
{
    async fn broadcast(&self, raw_tx: Vec<u8>) -> Result<TransactionId, BroadcastRejection> {
        // Forward to the passthrough provider — a one-line delegate, no routing
        // decision here. The classification (broadcast is remote) is that the
        // remote view carries this capability.
        self.remote.broadcast(raw_tx).await
    }
}

impl<Fs: Send + Sync + 'static, Nfs: Send + Sync + 'static, Src: Send + Sync + 'static> Serviceable
    for Engine<Fs, Nfs, Src>
{
    fn serviceability(&self) -> ServiceabilityManifest {
        ServiceabilityManifest::default()
    }
}

impl<Fs: Send + Sync + 'static, Nfs: Send + Sync + 'static, Src: Send + Sync + 'static>
    ReportedUpgrades for Engine<Fs, Nfs, Src>
{
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, ReadError> {
        // Follow-up: pass through the validator schedule.
        Ok(Vec::new())
    }
}

impl<Fs: Send + Sync + 'static, Nfs: Send + Sync + 'static, Src: Send + Sync + 'static> Passthrough
    for Engine<Fs, Nfs, Src>
{
    async fn passthrough(&self, _query: PassthroughQuery) -> Result<PassthroughAnswer, Transient> {
        // Follow-up: relay to the validator.
        Err(Transient("passthrough not wired yet".into()))
    }
}

impl<Fs, Nfs, Src> IndexerService for Engine<Fs, Nfs, Src>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead> + 'static,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead> + 'static,
    Src: GetTreestate + SendRawTransaction + Clone + 'static,
{
}

// --- coherence marker + served range (on the snapshot) -----------------------
//
// Delegated to the composed view: the pin, the coverage, and the finalised/tip
// boundary are exactly what the composer already computes across the seam.

impl<F, N, Src> ChainSegment for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    fn pinned_tip(&self) -> Option<BlockId> {
        self.local.pinned_tip()
    }

    fn coverage(&self) -> Option<HeightRange> {
        self.local.coverage()
    }
}

impl<F, N, Src> Snapshot for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    fn serviceable_range(&self) -> ServiceableRange {
        self.local.serviceable_range()
    }
}

// --- reads (on the snapshot) -------------------------------------------------
//
// Compact-block serving and chain-info are wired; the conversion-heavy per-block
// reads are NotServiceable until they are sourced.

impl<F, N, Src> CompactBlockRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        self.local.compact_block(at).await
    }
    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        self.local.stream_compact(range)
    }
}

impl<F, N, Src> BlockRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn tip(&self) -> Result<BlockId, BlockReadError> {
        // The composed tip — the NFS tip, falling back to the finalised tip.
        self.local
            .pinned_tip()
            .ok_or(BlockReadError::NotServiceable(Capability::Blocks))
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

impl<F, N, Src> TransactionRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn transaction(&self, _id: TransactionId) -> Result<Option<Transaction>, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::RawTransaction))
    }
    async fn transaction_status(&self, _id: TransactionId) -> Result<TxStatus, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::TransactionLocation))
    }
}

impl<F, N, Src> TreestateRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: GetTreestate + Send + Sync + 'static,
{
    async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        // Passthrough: zaino does not index treestate, so the remote view answers
        // it live. That treestate is remote is the read-set's per-capability
        // local/passthrough classification, expressed here.
        self.remote.treestate(at).await
    }
    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _range: HeightRange,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        // Still stubbed: the service asks by height range, the source by
        // subtree-index + limit — passthrough needs a translation, not a straight
        // relay. Wired once that mapping lands.
        Err(TreestateReadError::NotServiceable(Capability::SubtreeRoots))
    }
}

impl<F, N, Src> AddressRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
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

impl<F, N, Src> SpendRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        Err(SpendReadError::NotServiceable(Capability::SpendStatus))
    }
}

impl<F, N, Src> CompactNullifierRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn compact_block_nullifiers(
        &self,
        _at: BlockRef,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
}

impl<F, N, Src> ChainInfoRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn chain_info(&self) -> Result<ChainInfo, ReadError> {
        // Read from the composed pinned tip; an empty view falls back to genesis.
        let tip = self.local.pinned_tip();
        let estimated_height = tip.map(|id| id.height).unwrap_or_else(genesis);
        Ok(ChainInfo {
            tip,
            estimated_height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;
    use zaino_service::{
        ChainSegment, CompactBlockRead, LightServeService, NodeRpcService, TakeSnapshot,
        WalletLibService,
    };

    /// The milestone: the composed engine type-checks as every public profile,
    /// over any two composer inputs (each a `TakeSnapshot` whose snapshot is a
    /// `ChainSegment + CompactBlockRead`). Compile-time only.
    fn _engine_satisfies_all_profiles<Fs, Nfs, Src>()
    where
        Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
        Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
        Src: zaino_source::GetTreestate + zaino_source::SendRawTransaction + Clone + 'static,
        Engine<Fs, Nfs, Src>: WalletLibService + LightServeService + NodeRpcService,
    {
    }

    // Broadcast passthrough, exercised entirely with in-crate mocks: two empty
    // stub views for the composed chain, a `MockChain` for the validator source.
    // No cluster, no validator — the routing is deterministic.
    use zaino_chainview::testing::StubNonFinalised;
    use zaino_service::error::{BroadcastRejection, TreestateReadError};
    use zaino_service::{Broadcast, TreestateRead};
    use zaino_source::mock::MockChain;
    use zaino_source::{RetryPolicy, SendRawTransactionError, ValidatorClient};

    use super::Height;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid height")
    }

    // The engine consumes the *canonical* (resilient) source, so the mock is
    // wrapped in the ValidatorClient decorator — exactly how the root injects it.
    fn engine_with(
        source: MockChain,
    ) -> Engine<StubNonFinalised, StubNonFinalised, ValidatorClient<MockChain>> {
        Engine::new(
            StubNonFinalised::empty(),
            StubNonFinalised::empty(),
            ValidatorClient::new(source, RetryPolicy::default()),
        )
    }

    #[tokio::test]
    async fn broadcast_relays_to_the_source_and_returns_its_txid() {
        let engine = engine_with(MockChain::new());
        let raw = vec![7u8; 32];
        let txid = engine.broadcast(raw.clone()).await.expect("accepted");
        // The mock echoes the submitted bytes as the id, proving the exact
        // transaction reached the source's send port.
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&raw);
        assert_eq!(txid, super::TransactionId::from(expected));
    }

    #[tokio::test]
    async fn broadcast_maps_a_validator_rejection_to_invalid() {
        let engine = engine_with(
            MockChain::new().reject_send(SendRawTransactionError::Rejected("bad script".into())),
        );
        match engine.broadcast(vec![1, 2, 3]).await {
            Err(BroadcastRejection::Invalid(reason)) => assert_eq!(reason, "bad script"),
            other => panic!("expected an Invalid rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn broadcast_maps_malformed_bytes_to_malformed() {
        let engine = engine_with(
            MockChain::new().reject_send(SendRawTransactionError::Malformed("not a tx".into())),
        );
        match engine.broadcast(vec![0xff]).await {
            Err(BroadcastRejection::Malformed(reason)) => assert_eq!(reason, "not a tx"),
            other => panic!("expected a Malformed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn treestate_passes_through_a_missing_height() {
        // No treestate seeded: the mock answers HeightNotFound, which the remote
        // provider maps to a definitive read failure. Proves treestate routes to
        // the passthrough provider (the read-set's per-cap classification) — not a
        // NotServiceable stub.
        let engine = engine_with(MockChain::new());
        let snapshot = engine.snapshot().await.expect("snapshot acquired");
        match TreestateRead::treestate(&snapshot, height(5)).await {
            Err(TreestateReadError::Fatal(msg)) => {
                assert!(msg.contains("no treestate at height"), "got: {msg}")
            }
            other => panic!("expected a definitive miss, got {other:?}"),
        }
    }
}
