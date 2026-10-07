//! [`Snapshot`]: one served tip across every enabled index, what a request reads (`nfs.md` §6)
//!
//! - Pinned for a request's life: nodes through `Arc`, disk through the committed view
//! - Layers rebased onto the committed views they pair with, at build (decision 4)
//! - [`Snapshot::at`]: any folded node or the root, side branches included (G7)

use std::sync::Arc;

use tokio::sync::watch;
use zaino_header_chain::VerifiedChain;
use zaino_index_compact_block::CompactBlockReader;
use zaino_index_transparent_address::TransparentAddressReader;
use zaino_index_tree_state::{PoolActivations, TreeStateReader};
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_internal_value_balance::ValueBalanceReader;
use zaino_persistence::{IndexKind, Layer, LayeredView, MapRead, SequenceRead, View};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zaino_sync::PerIndex;
use zcash_protocol::consensus::NetworkType;

use crate::fold::{schema, Folded};
use crate::graph::{Base, Graph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    pub network: NetworkType,
    pub activations: PoolActivations,
}

#[cfg(any(test, feature = "testing"))]
impl ChainParams {
    /// What a validator following `chain` at `tip` reports (`getblockchaininfo`)
    pub fn of(chain: &zaino_primitives::testing::MockChain, tip: BlockRef) -> Self {
        let activations = PoolActivations::from_validator(&chain.blockchain_info(tip));
        Self { network: chain.schedule().network, activations }
    }
}

/// Which branch of the snapshot's chain a block sits on (`from` = the side branch's best parent)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Best,
    Side { from: BlockRef },
}

/// Every enabled index as of one block: what a route reads
pub struct At<V> {
    block: BlockRef,
    branch: Branch,
    params: ChainParams,
    views: Views<V>,
}

impl<V> At<V> {
    /// `GetLatestBlock`: every read through it answers at heights `<=` it
    pub fn tip(&self) -> BlockRef {
        self.block
    }

    pub fn branch(&self) -> Branch {
        self.branch
    }

    pub fn params(&self) -> ChainParams {
        self.params
    }

    pub fn views(&self) -> &Views<V> {
        &self.views
    }
}

/// One publish: the served tip + every folded node, judged under `chain`
///
/// - `served` = deepest folded best block, else `root` (lowest durable tip)
/// - `durable` = each index's committed view as of this publish (republished per commit)
pub struct Snapshot<V> {
    chain: Arc<VerifiedChain>,
    root: Option<BlockRef>,
    served: At<V>,
    durable: PerIndex<V>,
    graph: Graph<Folded>,
}

impl<V: View> Snapshot<V> {
    /// Panics: `tip` neither a node of `graph` nor `root`
    pub(crate) fn new(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        root: Option<BlockRef>,
        params: ChainParams,
        durable: PerIndex<V>,
        graph: Graph<Folded>,
    ) -> Self {
        let base = graph.at(&chain, root, &tip.hash).expect("served tip = a node or the root");
        let served = at(params, &durable, base);
        Self { chain, root, served, durable, graph }
    }

    /// Index state as of `hash`: a folded node (best or side) or the root, no I/O
    ///
    /// - `None` = final below the root, never folded, or unknown
    /// - an index durable at or past the block: its committed view alone (reads at heights `<=`)
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>> {
        let base = self.graph.at(&self.chain, self.root, hash)?;
        Some(at(self.served.params, &self.durable, base))
    }

    /// Each enabled index's durable tip, subscribe order
    pub fn durable(&self) -> impl Iterator<Item = (IndexKind, Option<BlockRef>)> + '_ {
        self.durable.iter().map(|(kind, view)| (kind, view.tip()))
    }
}

impl<V> Snapshot<V> {
    pub fn chain(&self) -> &Arc<VerifiedChain> {
        &self.chain
    }

    pub fn served(&self) -> &At<V> {
        &self.served
    }

    /// = `served().tip()`
    pub fn tip(&self) -> BlockRef {
        self.served.block
    }

    pub fn params(&self) -> ChainParams {
        self.served.params
    }

    /// = `served().views()`
    pub fn views(&self) -> &Views<V> {
        &self.served.views
    }

    /// `hash` = a node folded in this snapshot (the root excluded)
    pub fn folded(&self, hash: &BlockHash) -> bool {
        self.graph.contains(hash)
    }
}

fn at<V: View>(params: ChainParams, durable: &PerIndex<V>, base: Base<Folded>) -> At<V> {
    let layers = base.folded.as_deref().map(|folded| &folded.layers);
    let views = Views::at(params.network, durable, layers, base.at.height);
    At { block: base.at, branch: base.branch, params, views }
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

    /// As of the block at `height`: `layers` (`None` = the root), empty for an index durable at
    /// or past it (a layer rebases only onto one of its own blocks)
    fn at(
        network: NetworkType,
        durable: &PerIndex<V>,
        layers: Option<&PerIndex<Layer>>,
        height: Height,
    ) -> Self {
        let mut chosen = PerIndex::default();
        for (kind, view) in durable.iter() {
            let holds = view.tip().is_some_and(|tip| tip.height >= height);
            let layer = match layers.filter(|_| !holds) {
                Some(layers) => {
                    layers.get(kind).unwrap_or_else(|| panic!("{}: no layer", kind.name())).clone()
                }
                None => Layer::empty(&schema(kind, network)),
            };
            chosen.insert(kind, layer);
        }
        Self::new(network, durable, &chosen)
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

/// Latest [`Snapshot`] (`None` = nothing published yet), swapped whole per publish
pub type Published<V> = watch::Receiver<Option<Arc<Snapshot<V>>>>;

/// Reader's end: the latest [`Snapshot`], swapped whole per publish
pub struct NfsHandle<V> {
    published: Published<V>,
}

impl<V> Clone for NfsHandle<V> {
    fn clone(&self) -> Self {
        Self { published: self.published.clone() }
    }
}

impl<V> NfsHandle<V> {
    /// One load (`None` = nothing published yet)
    pub fn snapshot(&self) -> Option<Arc<Snapshot<V>>> {
        self.published.borrow().clone()
    }

    /// Next publish (`Err` = the driver stopped)
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.published.changed().await
    }
}

/// Snapshots and handles no driver publishes into (consumers' tests)
#[cfg(any(test, feature = "testing"))]
impl<V: View> Snapshot<V> {
    /// At `tip` on `chain`, `tip` = the root: each enabled index = its committed view, no layer
    pub fn fixed(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let mut views = PerIndex::default();
        durable.into_iter().for_each(|(kind, view)| views.insert(kind, view));
        Self::new(chain, tip, Some(tip), params, views, Graph::new())
    }
}

#[cfg(any(test, feature = "testing"))]
impl<V: View> NfsHandle<V> {
    /// Nothing published yet (a booting NFS)
    pub fn unpublished() -> Self {
        Publisher::new().handle()
    }

    /// [`Snapshot::fixed`] for good
    pub fn fixed(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let published = Publisher::new();
        published.publish(Snapshot::fixed(chain, tip, params, durable));
        published.handle()
    }
}

/// Driver's end of every [`NfsHandle`] and [`Published`] watch
pub(crate) struct Publisher<V> {
    published: watch::Sender<Option<Arc<Snapshot<V>>>>,
}

impl<V> Publisher<V> {
    pub(crate) fn new() -> Self {
        Self { published: watch::channel(None).0 }
    }

    pub(crate) fn handle(&self) -> NfsHandle<V> {
        NfsHandle { published: self.published.subscribe() }
    }

    pub(crate) fn subscribe(&self) -> Published<V> {
        self.published.subscribe()
    }

    pub(crate) fn publish(&self, snapshot: Snapshot<V>) {
        self.published.send_replace(Some(Arc::new(snapshot)));
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::testing::{h, MockChain, Upgrades};
    use zcash_protocol::consensus::NetworkUpgrade;

    use super::*;

    /// Activations = the chain's schedule as its validator reports it (pending ones included);
    /// network = the chain's label
    #[test]
    fn chain_params_are_what_a_validator_following_the_chain_reports() {
        let upgrades = Upgrades::all_at(h(1)).onward(NetworkUpgrade::Nu5, h(3));
        let schedule = upgrades.without(NetworkUpgrade::Nu6_3);
        let mut chain = MockChain::regtest().upgrades(schedule).network(NetworkType::Main);
        let tip = chain.mine_empty(2);
        let activations = PoolActivations { sapling: h(1), orchard: Some(h(3)), ironwood: None };
        let expected = ChainParams { network: NetworkType::Main, activations };
        assert_eq!(ChainParams::of(&chain, tip), expected);
    }
}
