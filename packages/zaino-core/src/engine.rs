//! [`Engine`] — the engine, composed under a use case's routing.
//!
//! Three providers, one routing type. The finalised store `Fs` and the
//! non-finalised head `Nfs` are the local chain tiers, captured together on
//! each pin by [`ChainView`]; the validator handle `Src` is the passthrough
//! provider, answered live through [`PassthroughProvider`]. `R` is the use case's
//! [`Routing`]: for every capability whose placement is a decision, which of
//! those providers answers it.
//!
//! The read surface is [`EngineSnapshot`]. Each read trait is implemented
//! on it **once per placement**, bounded on `R`'s placement for that
//! capability and on the provider ports that placement needs. So a read the
//! providers cannot back under the chosen routing is not a stub that refuses
//! at runtime: it is an impl that does not exist, and the use case's demand
//! bound fails where the engine is wired.
//!
//! # Presence is checked at the wiring
//!
//! The light table over the light-wallet index set serves the light-wallet
//! use case:
//!
//! ```
//! use zaino_indexes::sets::compact_blocks::CompactBlocks;
//! use zaino_persistence::in_memory::InMemoryBackend;
//! use zaino_core::routing::LightWalletRouting;
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightWalletService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store::StoreReader;
//! use zaino_core::Engine;
//!
//! fn wired<S: LightWalletService>() {}
//! wired::<Engine<
//!     StoreReader<InMemoryBackend, CompactBlocks>,
//!     MockIndexerService,
//!     ValidatorClient<MockChain>,
//!     LightWalletRouting,
//! >>();
//! ```
//!
//! Route address history locally instead, over the same store, and it is not —
//! `CompactBlocks` does not build `address_history`, so the store has no local
//! address read for the composer to merge with the head's:
//!
//! ```compile_fail,E0277
//! use zaino_indexes::sets::compact_blocks::CompactBlocks;
//! use zaino_persistence::in_memory::InMemoryBackend;
//! use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightWalletService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_store::StoreReader;
//! use zaino_core::Engine;
//!
//! struct AddressLocal;
//! impl Routing for AddressLocal {
//!     type Address = Local;
//!     type Treestate = Passthrough;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightWalletService>() {}
//! wired::<Engine<
//!     StoreReader<InMemoryBackend, CompactBlocks>,
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
//! use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightWalletService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_core::Engine;
//!
//! struct AddressLocal;
//! impl Routing for AddressLocal {
//!     type Address = Local;
//!     type Treestate = Passthrough;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightWalletService>() {}
//! wired::<Engine<
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
//! use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
//! use zaino_service::testing::MockIndexerService;
//! use zaino_service::LightWalletService;
//! use zaino_source::mock::MockChain;
//! use zaino_source::ValidatorClient;
//! use zaino_core::Engine;
//!
//! struct TreestateLocal;
//! impl Routing for TreestateLocal {
//!     type Address = Passthrough;
//!     type Treestate = Local;
//!     type Spend = Withheld;
//!     type TransactionLocation = Withheld;
//! }
//!
//! fn wired<S: LightWalletService>() {}
//! wired::<Engine<
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

pub use snapshot::EngineSnapshot;
#[cfg(test)]
pub(crate) use snapshot::split_at_seam;

use std::marker::PhantomData;

use futures::stream::{self, BoxStream, StreamExt};

use crate::chain_view::ChainView;
use crate::routing::{PlacementKind, Routing};
use zaino_primitives::types::{PreIndexCompactTx, TransactionId};
use zaino_service::error::{BroadcastRejection, MempoolReadError, ReadError, Transient};
use zaino_service::{
    Answerable, MempoolTx, NodeQuery, NodeQueryAnswer, ReportedUpgrade, ServiceabilityManifest,
    TipEvent,
};
use zaino_service::{
    Broadcast, ChainSegment, CompactBlockRead, IndexerService, MempoolContent, MempoolSubscribe,
    NodeQueryRelay, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};
use zaino_source::{
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetTreestate, SendRawTransaction,
};

use crate::passthrough::PassthroughProvider;

/// The engine: the local chain tiers and the validator, composed under the
/// routing `R`.
///
/// `Fs` and `Nfs` are each a [`TakeSnapshot`] whose pin is a coherence
/// coordinate and a compact-block read; both are captured together on each
/// [`snapshot`](TakeSnapshot::snapshot), so a read through the returned
/// [`EngineSnapshot`] is coherent across the seam. `Src` is the resilient
/// validator handle, bound through the canonical `zaino-source` ports.
pub struct Engine<Fs, Nfs, Src, R> {
    view: ChainView<Fs, Nfs>,
    passthrough: PassthroughProvider<Src>,
    routing: PhantomData<R>,
}

impl<Fs: Clone, Nfs: Clone, Src: Clone, R> Clone for Engine<Fs, Nfs, Src, R> {
    fn clone(&self) -> Self {
        Self {
            view: self.view.clone(),
            passthrough: self.passthrough.clone(),
            routing: PhantomData,
        }
    }
}

impl<Fs, Nfs, Src, R> Engine<Fs, Nfs, Src, R>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    R: Routing,
{
    /// Compose a finalised store, a non-finalised head, and the validator
    /// handle under the routing `R`.
    ///
    /// Nothing here decides which provider answers what: `R` does, and the
    /// read impls on [`EngineSnapshot`] carry that decision as their bounds.
    pub fn new(fs: Fs, nfs: Nfs, source: Src) -> Self {
        Self {
            view: ChainView::new(fs, nfs),
            passthrough: PassthroughProvider::new(source),
            routing: PhantomData,
        }
    }
}

impl<Fs, Nfs, Src, R> TakeSnapshot for Engine<Fs, Nfs, Src, R>
where
    Fs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    type Snapshot = EngineSnapshot<Fs::Snapshot, Nfs::Snapshot, Src, R>;

    async fn snapshot(&self) -> Result<Self::Snapshot, Transient> {
        // The composer captures both local sides in one shot, so the pin is
        // coherent across the seam. The passthrough handle rides along for
        // passthrough reads, which are live, not pinned.
        Ok(EngineSnapshot::new(
            self.view.snapshot().await?,
            self.passthrough.clone(),
        ))
    }
}

impl<Fs, Nfs, Src, R> TipSubscribe for Engine<Fs, Nfs, Src, R>
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

impl<Fs, Nfs, Src, R> MempoolSubscribe for Engine<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: GetMempoolTxids + GetMempoolSourceTip + Clone + Send + Sync + 'static,
    R: Routing,
{
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        let passthrough = self.passthrough.clone();
        // A snapshot of the mempool delivered as a finite stream: read the
        // coherence tip and the listing from the one source and tag each txid
        // with the tip. Passthrough has no live push — the dedicated mempool
        // component provides continuous updates later, behind this same port.
        // On any read failure the stream yields nothing rather than an error,
        // per the infallible `MempoolTx` stream contract.
        stream::once(async move {
            match (
                passthrough.mempool_source_tip().await,
                passthrough.mempool_txids().await,
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

impl<Fs, Nfs, Src, R> MempoolContent for Engine<Fs, Nfs, Src, R>
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
        self.passthrough.raw_mempool_transaction(txid).await
    }

    async fn mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<PreIndexCompactTx>, MempoolReadError> {
        // Same live passthrough; the compact projection is done in the adapter.
        self.passthrough.mempool_compact_transaction(txid).await
    }
}

impl<Fs, Nfs, Src, R> Broadcast for Engine<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: SendRawTransaction + 'static,
    R: Routing,
{
    async fn broadcast(&self, raw_tx: Vec<u8>) -> Result<TransactionId, BroadcastRejection> {
        // Always the validator's: no local provider can relay. Not on
        // `Routing` because there is nothing to decide.
        self.passthrough.broadcast(raw_tx).await
    }
}

/// The manifest is derived from the routing and the finalised store's own
/// manifest — the same declaration the reads are bounded on, so what is
/// advertised and what is served cannot disagree.
///
/// ```text
/// manifest(C) = Absent          if R::C = Withheld
///             | Live            if R::C = Passthrough
///             | fs.manifest(C)  if R::C = Local
/// ```
///
/// Reach through the non-finalised window is not folded in yet: a local
/// capability reports the finalised tip, though compact blocks are served up to
/// the head's tip. Serviceability sits on the engine while the head's tip is a
/// property of the pin, so widening it means moving this port onto the
/// snapshot — a separate decision. A passthrough capability reports `Live`
/// unconditionally: the passthrough provider is always wired here; folding in
/// the validator's reachability is where the runtime's probe joins.
impl<Fs, Nfs, Src, R> Serviceable for Engine<Fs, Nfs, Src, R>
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
            PlacementKind::Passthrough => Answerable::Live,
            PlacementKind::Local => local.get(capability),
        })
    }
}

impl<Fs, Nfs, Src, R> ReportedUpgrades for Engine<Fs, Nfs, Src, R>
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

impl<Fs, Nfs, Src, R> NodeQueryRelay for Engine<Fs, Nfs, Src, R>
where
    Fs: Send + Sync + 'static,
    Nfs: Send + Sync + 'static,
    Src: Send + Sync + 'static,
    R: Routing,
{
    async fn relay_node_query(&self, _query: NodeQuery) -> Result<NodeQueryAnswer, Transient> {
        // Follow-up: relay to the validator.
        Err(Transient("passthrough not wired yet".into()))
    }
}

impl<Fs, Nfs, Src, R> IndexerService for Engine<Fs, Nfs, Src, R>
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
