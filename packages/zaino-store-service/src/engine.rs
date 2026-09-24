//! [`Engine`] — the composed engine and its control surface.
//!
//! `Engine` pairs the composed FS⊕NFS chain (the local [`RemoteChainView`]'s
//! sibling, [`ChainView`]) with the passthrough provider and dresses the pair as
//! the inner service's controls: the pin, broadcast, the streaming
//! subscriptions, and the node-operator passthrough. The read surface it hands
//! out is [`EngineSnapshot`].

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chainview::ChainView;
use zaino_source::{
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetTreestate, SendRawTransaction,
};

use zaino_core::{
    MempoolTx, PassthroughAnswer, PassthroughQuery, PreIndexCompactTx, ReportedUpgrade,
    ServiceabilityManifest, TipEvent, TransactionId,
};
use zaino_service::error::{BroadcastRejection, MempoolReadError, ReadError, Transient};
use zaino_service::{
    Broadcast, ChainSegment, CompactBlockRead, IndexerService, MempoolContent, MempoolSubscribe,
    Passthrough, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};

use crate::remote::RemoteChainView;
use crate::snapshot::EngineSnapshot;

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
        Ok(EngineSnapshot::new(
            self.view.snapshot().await?,
            self.remote.clone(),
        ))
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

impl<Fs, Nfs, Src> MempoolSubscribe for Engine<Fs, Nfs, Src>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: GetMempoolTxids + GetMempoolSourceTip + Clone + Send + Sync + 'static,
{
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        let remote = self.remote.clone();
        // A snapshot of the mempool delivered as a finite stream: read the
        // coherence tip and the listing from the one source and tag each txid
        // with the tip. Passthrough has no live push — the dedicated mempool
        // component provides continuous updates later, behind this same port. On
        // any read failure the stream yields nothing rather than an error, per the
        // infallible `MempoolTx` stream contract.
        stream::once(async move {
            match (
                remote.mempool_source_tip().await,
                remote.mempool_txids().await,
            ) {
                (Ok(tip), Ok(txids)) => {
                    stream::iter(txids.into_iter().map(move |txid| MempoolTx {
                        txid,
                        validated_against: tip,
                    }))
                    .left_stream()
                }
                _ => stream::empty().right_stream(),
            }
        })
        .flatten()
        .boxed()
    }
}

impl<Fs, Nfs, Src> MempoolContent for Engine<Fs, Nfs, Src>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: GetRawMempoolTransaction + GetMempoolCompactTransaction + Send + Sync + 'static,
{
    async fn mempool_raw_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<Vec<u8>>, MempoolReadError> {
        // Live passthrough to the mempool's own source — never the finalised
        // secondary, which holds no mempool. Routing lives in the source adapter.
        self.remote.raw_mempool_transaction(txid).await
    }

    async fn mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<PreIndexCompactTx>, MempoolReadError> {
        // Same live passthrough; the compact projection is done in the adapter.
        self.remote.mempool_compact_transaction(txid).await
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
    Src: GetTreestate
        + SendRawTransaction
        + GetMempoolTxids
        + GetRawMempoolTransaction
        + GetMempoolCompactTransaction
        + GetMempoolSourceTip
        + Clone
        + 'static,
{
}
