//! [`Snapshot`]: one served tip across every enabled index, what a request reads (`nfs.md` §6)
//!
//! - Pinned for a request's life: nodes through `Arc`, disk through the committed view
//! - Layers rebased onto the committed views they pair with, at build (decision 4)

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use tokio::sync::watch;
use zaino_header_chain::VerifiedChain;
use zaino_index_compact_block::CompactBlockReader;
use zaino_index_transparent_address::TransparentAddressReader;
use zaino_index_tree_state::{PoolActivations, TreeStateReader};
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_internal_value_balance::ValueBalanceReader;
use zaino_persistence::{IndexKind, Layer, LayeredView, MapRead, SequenceRead, View};
use zaino_primitives::types::BlockRef;
use zaino_sync::PerIndex;
use zcash_protocol::consensus::NetworkType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    pub network: NetworkType,
    pub activations: PoolActivations,
}

/// `tip` = folded and on `chain`'s best, else the root
pub struct Snapshot<V> {
    pub(crate) chain: Arc<VerifiedChain>,
    pub(crate) tip: BlockRef,
    pub(crate) params: ChainParams,
    pub(crate) views: Views<V>,
}

impl<V> Snapshot<V> {
    pub fn chain(&self) -> &Arc<VerifiedChain> {
        &self.chain
    }

    /// `GetLatestBlock`: every read through this snapshot answers at heights `<=` it
    pub fn tip(&self) -> BlockRef {
        self.tip
    }

    pub fn params(&self) -> ChainParams {
        self.params
    }

    pub fn views(&self) -> &Views<V> {
        &self.views
    }
}

/// Every enabled index as of one block: its committed view + its layer above it
///
/// - Snapshot's state, and a fold's parent
#[derive(Clone)]
pub struct Views<V> {
    network: NetworkType,
    durable: PerIndex<V>,
    layers: PerIndex<Layer>,
}

impl<V: View> Views<V> {
    /// Panics: an index without a layer, or a layer off `durable`'s chain (`Layer::rebase`)
    pub(crate) fn new(
        network: NetworkType,
        durable: &PerIndex<V>,
        layers: &PerIndex<Layer>,
    ) -> Self {
        let mut rebased = PerIndex::default();
        for (kind, view) in durable.iter() {
            let layer = layers.get(kind).unwrap_or_else(|| panic!("{}: no layer", kind.name()));
            rebased.insert(kind, layer.rebase(view));
        }
        Self { network, durable: durable.clone(), layers: rebased }
    }

    pub(crate) fn network(&self) -> NetworkType {
        self.network
    }

    pub(crate) fn enabled(&self, kind: IndexKind) -> bool {
        self.durable.get(kind).is_some()
    }

    /// Panics: `kind` disabled
    pub(crate) fn layer(&self, kind: IndexKind) -> &Layer {
        self.layers.get(kind).unwrap_or_else(|| panic!("{}: disabled", kind.name()))
    }

    pub(crate) fn view(&self, kind: IndexKind) -> Option<LayeredView<V>> {
        let durable = self.durable.get(kind)?.clone();
        Some(LayeredView::new(durable, self.layer(kind).clone()))
    }
}

impl<V: SequenceRead> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<LayeredView<V>>> {
        let view = self.view(IndexKind::CompactBlock)?;
        Some(CompactBlockReader::new(view, self.network))
    }

    pub fn tree_state(&self) -> Option<TreeStateReader<LayeredView<V>>> {
        Some(TreeStateReader::new(self.view(IndexKind::TreeState)?, self.network))
    }
}

impl<V: MapRead> Views<V> {
    pub fn block_hash(&self) -> Option<BlockHashReader<LayeredView<V>>> {
        Some(BlockHashReader::new(self.view(IndexKind::BlockHash)?))
    }

    pub fn transparent_address(&self) -> Option<TransparentAddressReader<LayeredView<V>>> {
        let view = self.view(IndexKind::TransparentAddress)?;
        Some(TransparentAddressReader::new(view, self.network))
    }

    /// Fold parent only (no route reads value-balance)
    pub(crate) fn value_balance(&self) -> Option<ValueBalanceReader<LayeredView<V>>> {
        let view = self.view(IndexKind::ValueBalance)?;
        Some(ValueBalanceReader::new(view, self.network))
    }
}

/// Reader's end: the latest [`Snapshot`], swapped whole per publish
pub struct NfsHandle<V> {
    current: Arc<ArcSwapOption<Snapshot<V>>>,
    changed: watch::Receiver<()>,
}

impl<V> Clone for NfsHandle<V> {
    fn clone(&self) -> Self {
        Self { current: Arc::clone(&self.current), changed: self.changed.clone() }
    }
}

impl<V> NfsHandle<V> {
    /// One atomic load (`None` = nothing published yet)
    pub fn snapshot(&self) -> Option<Arc<Snapshot<V>>> {
        self.current.load_full()
    }

    /// Next publish (`Err` = the driver stopped)
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.changed.changed().await
    }
}

/// Handles no driver publishes into (consumers' route tests)
#[cfg(any(test, feature = "testing"))]
impl<V: View> NfsHandle<V> {
    /// Nothing published yet (a booting NFS)
    pub fn unpublished() -> Self {
        Publisher::new().handle()
    }

    /// One snapshot for good at `tip` on `chain`: each enabled index = its committed view, no layer
    pub fn fixed(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let (mut views, mut layers) = (PerIndex::default(), PerIndex::default());
        for (kind, view) in durable {
            layers.insert(kind, Layer::empty(&crate::fold::schema(kind, params.network)));
            views.insert(kind, view);
        }
        let views = Views::new(params.network, &views, &layers);
        let published = Publisher::new();
        published.publish(Snapshot { chain, tip, params, views });
        published.handle()
    }
}

/// Driver's end of every [`NfsHandle`]
pub(crate) struct Publisher<V> {
    current: Arc<ArcSwapOption<Snapshot<V>>>,
    changed: watch::Sender<()>,
}

impl<V> Publisher<V> {
    pub(crate) fn new() -> Self {
        Self { current: Arc::default(), changed: watch::channel(()).0 }
    }

    pub(crate) fn handle(&self) -> NfsHandle<V> {
        NfsHandle { current: Arc::clone(&self.current), changed: self.changed.subscribe() }
    }

    pub(crate) fn publish(&self, snapshot: Snapshot<V>) {
        self.current.store(Some(Arc::new(snapshot)));
        self.changed.send_replace(());
    }
}
