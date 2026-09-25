//! [`Composed`] — the engine, composed under a use case's routing.
//!
//! Three providers, one routing type. The finalised store `Fs` and the
//! non-finalised head `Nfs` are the local chain tiers, captured together on
//! each pin by [`ChainView`]; the validator handle `Src` is the passthrough
//! provider, answered live through [`RemoteChainView`]. `R` is the use case's
//! [`Routing`]: for every capability whose placement is a decision, which of
//! those providers answers it.
//!
//! The read surface is [`ComposedSnapshot`]. Each read trait is implemented
//! on it **once per placement**, bounded on `R`'s placement for that
//! capability and on the provider ports that placement needs. So a read the
//! providers cannot back under the chosen routing is not a stub that refuses
//! at runtime: it is an impl that does not exist, and the use case's profile
//! bound fails where the engine is wired.
//!
//! # Presence is checked at the wiring
//!
//! The light table over the light materialisation is the light profile:
//!
//! ```
//! use zaino_indexes::sets::light_wallet::LightWallet;
//! use zaino_persistence::in_memory::InMemoryBackend;
//! use zaino_service::routing::LightRouting;
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightServeService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store::StoreReader;
//! use zaino_store_service::Composed;
//!
//! fn wired<S: LightServeService>() {}
//! wired::<Composed<
//!     StoreReader<InMemoryBackend, LightWallet>,
//!     MockIndexerService,
//!     ValidatorClient<MockChain>,
//!     LightRouting,
//! >>();
//! ```
//!
//! Route address history locally instead, over the same store, and it is not —
//! `LightWallet` does not build `address_history`, so the store has no local
//! address read for the composer to merge with the head's:
//!
//! ```compile_fail,E0277
//! use zaino_indexes::sets::light_wallet::LightWallet;
//! use zaino_persistence::in_memory::InMemoryBackend;
//! use zaino_service::routing::{Local, Remote, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightServeService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store::StoreReader;
//! use zaino_store_service::Composed;
//!
//! struct AddressLocal;
//! impl Routing for AddressLocal {
//!     type Address = Local;
//!     type Treestate = Remote;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightServeService>() {}
//! wired::<Composed<
//!     StoreReader<InMemoryBackend, LightWallet>,
//!     MockIndexerService,
//!     ValidatorClient<MockChain>,
//!     AddressLocal,
//! >>();
//! ```
//!
//! The same local routing over providers that *do* back address history on
//! both sides is fine — the bound is on the providers, not the placement:
//!
//! ```
//! use zaino_service::routing::{Local, Remote, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightServeService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store_service::Composed;
//!
//! struct AddressLocal;
//! impl Routing for AddressLocal {
//!     type Address = Local;
//!     type Treestate = Remote;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightServeService>() {}
//! wired::<Composed<
//!     MockIndexerService,
//!     MockIndexerService,
//!     ValidatorClient<MockChain>,
//!     AddressLocal,
//! >>();
//! ```
//!
//! And a placement no provider can take at all — treestate has no local
//! index on any tier — is an impl that does not exist for any providers:
//!
//! ```compile_fail,E0277
//! use zaino_service::routing::{Local, Remote, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightServeService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store_service::Composed;
//!
//! struct TreestateLocal;
//! impl Routing for TreestateLocal {
//!     type Address = Remote;
//!     type Treestate = Local;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightServeService>() {}
//! wired::<Composed<
//!     MockIndexerService,
//!     MockIndexerService,
//!     ValidatorClient<MockChain>,
//!     TreestateLocal,
//! >>();
//! ```

mod address;
mod snapshot;
mod spend;
mod treestate;

#[cfg(test)]
pub(crate) use snapshot::split_at_seam;
pub use snapshot::ComposedSnapshot;

use std::marker::PhantomData;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chainview::ChainView;
use zaino_core::{
    Answerable, MempoolTx, PassthroughAnswer, PassthroughQuery, PreIndexCompactTx, ReportedUpgrade,
    ServiceabilityManifest, TipEvent, TransactionId,
};
use zaino_service::error::{BroadcastRejection, MempoolReadError, ReadError, Transient};
use zaino_service::routing::{PlacementKind, Routing};
use zaino_service::{
    Broadcast, ChainSegment, CompactBlockRead, IndexerService, MempoolContent, MempoolSubscribe,
    Passthrough, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};
use zaino_source::{
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetTreestate, SendRawTransaction,
};

use crate::remote::RemoteChainView;

/// The engine: the local chain tiers and the validator, composed under the
/// routing `R`.
///
/// `Fs` and `Nfs` are each a [`TakeSnapshot`] whose pin is a coherence
/// coordinate and a compact-block read; both are captured together on each
/// [`snapshot`](TakeSnapshot::snapshot), so a read through the returned
/// [`ComposedSnapshot`] is coherent across the seam. `Src` is the resilient
/// validator handle, bound through the canonical `zaino-source` ports.
pub struct Composed<Fs, Nfs, Src, R> {
    view: ChainView<Fs, Nfs>,
    remote: RemoteChainView<Src>,
    routing: PhantomData<R>,
}

impl<Fs: Clone, Nfs: Clone, Src: Clone, R> Clone for Composed<Fs, Nfs, Src, R> {
    fn clone(&self) -> Self {
        Self {
            view: self.view.clone(),
            remote: self.remote.clone(),
            routing: PhantomData,
        }
    }
}

impl<Fs, Nfs, Src, R> Composed<Fs, Nfs, Src, R>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    R: Routing,
{
    /// Compose a finalised store, a non-finalised head, and the validator
    /// handle under the routing `R`.
    ///
    /// Nothing here decides which provider answers what: `R` does, and the
    /// read impls on [`ComposedSnapshot`] carry that decision as their bounds.
    pub fn new(fs: Fs, nfs: Nfs, source: Src) -> Self {
        Self {
            view: ChainView::new(fs, nfs),
            remote: RemoteChainView::new(source),
            routing: PhantomData,
        }
    }
}

impl<Fs, Nfs, Src, R> TakeSnapshot for Composed<Fs, Nfs, Src, R>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    type Snapshot = ComposedSnapshot<Fs::Snapshot, Nfs::Snapshot, Src, R>;

    async fn snapshot(&self) -> Result<Self::Snapshot, Transient> {
        // The composer captures both local sides in one shot, so the pin is
        // coherent across the seam. The remote handle rides along for
        // passthrough reads, which are live, not pinned.
        Ok(ComposedSnapshot::new(
            self.view.snapshot().await?,
            self.remote.clone(),
        ))
    }
}

impl<Fs, Nfs, Src, R> TipSubscribe for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: Send + Sync + 'static,
    R: Routing,
{
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent> {
        // Follow-up: bridge the chain-head epoch watch to tip events.
        stream::empty().boxed()
    }
}

impl<Fs, Nfs, Src, R> MempoolSubscribe for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: GetMempoolTxids + GetMempoolSourceTip + Clone + Send + Sync + 'static,
    R: Routing,
{
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        let remote = self.remote.clone();
        // A snapshot of the mempool delivered as a finite stream: read the
        // coherence tip and the listing from the one source and tag each txid
        // with the tip. Passthrough has no live push — the dedicated mempool
        // component provides continuous updates later, behind this same port.
        // On any read failure the stream yields nothing rather than an error,
        // per the infallible `MempoolTx` stream contract.
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

impl<Fs, Nfs, Src, R> MempoolContent for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: GetRawMempoolTransaction + GetMempoolCompactTransaction + Send + Sync + 'static,
    R: Routing,
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

impl<Fs, Nfs, Src, R> Broadcast for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: SendRawTransaction + 'static,
    R: Routing,
{
    async fn broadcast(&self, raw_tx: Vec<u8>) -> Result<TransactionId, BroadcastRejection> {
        // Always the validator's: no local provider can relay. Not on
        // `Routing` because there is nothing to decide.
        self.remote.broadcast(raw_tx).await
    }
}

/// The manifest is derived from the routing and the finalised store's own
/// manifest — the same declaration the reads are bounded on, so what is
/// advertised and what is served cannot disagree.
///
/// ```text
/// manifest(C) = Absent          if R::C = Withheld
///             | Live            if R::C = Remote
///             | fs.manifest(C)  if R::C = Local
/// ```
///
/// Reach through the non-finalised window is not folded in yet: a local
/// capability reports the finalised tip, though compact blocks are served up to
/// the head's tip. Serviceability sits on the engine while the head's tip is a
/// property of the pin, so widening it means moving this port onto the
/// snapshot — a separate decision. A remote capability reports `Live`
/// unconditionally: the passthrough provider is always wired here; folding in
/// the validator's reachability is where the runtime's probe joins.
impl<Fs, Nfs, Src, R> Serviceable for Composed<Fs, Nfs, Src, R>
where
    Fs: Serviceable,
    Nfs: Send + Sync + 'static,
    Src: Send + Sync + 'static,
    R: Routing,
{
    fn serviceability(&self) -> ServiceabilityManifest {
        let local = self.view.finalised().serviceability();
        ServiceabilityManifest::derive(|capability| match R::placement(capability) {
            PlacementKind::Withheld => Answerable::Absent,
            PlacementKind::Remote => Answerable::Live,
            PlacementKind::Local => local.get(capability),
        })
    }
}

impl<Fs, Nfs, Src, R> ReportedUpgrades for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: Send + Sync + 'static,
    R: Routing,
{
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, ReadError> {
        // Follow-up: pass through the validator schedule.
        Ok(Vec::new())
    }
}

impl<Fs, Nfs, Src, R> Passthrough for Composed<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: Send + Sync + 'static,
    R: Routing,
{
    async fn passthrough(&self, _query: PassthroughQuery) -> Result<PassthroughAnswer, Transient> {
        // Follow-up: relay to the validator.
        Err(Transient("passthrough not wired yet".into()))
    }
}

impl<Fs, Nfs, Src, R> IndexerService for Composed<Fs, Nfs, Src, R>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead> + Serviceable + 'static,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead> + 'static,
    Src: GetTreestate
        + SendRawTransaction
        + GetMempoolTxids
        + GetRawMempoolTransaction
        + GetMempoolCompactTransaction
        + GetMempoolSourceTip
        + Clone
        + 'static,
    R: Routing,
{
}
