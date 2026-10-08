//! [`Indexed`]: one served tip across every enabled index, the global snapshot's index half
//! (`nfs.md`)
//!
//! - Pinned for a request's life: nodes through `Arc`, disk through the committed view
//! - Layers rebased onto the committed views they pair with, at build (decision 4)
//! - [`Indexed::at`]: any folded node or the root, side branches included (G7)

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

use crate::fold::Folded;
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
/// - `served` = deepest folded best block, else `root` (lowest durable tip of the serving indexes)
/// - `durable` = each enabled index's committed view as of this publish (republished per commit)
/// - `serving` = indexes served (the rest: absent, [`Views::syncing`])
pub struct Indexed<V> {
    chain: Arc<VerifiedChain>,
    root: Option<BlockRef>,
    served: At<V>,
    durable: PerIndex<V>,
    serving: Vec<IndexKind>,
    graph: Graph<Folded>,
}

impl<V: View> Indexed<V> {
    /// Panics: `tip` neither a node of `graph` nor `root`
    pub(crate) fn new(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        root: Option<BlockRef>,
        params: ChainParams,
        durable: PerIndex<V>,
        serving: Vec<IndexKind>,
        graph: Graph<Folded>,
    ) -> Self {
        let base = graph.at(&chain, root, &tip.hash).expect("served tip = a node or the root");
        let served = at(params, &durable, &serving, base);
        Self { chain, root, served, durable, serving, graph }
    }

    /// Index state as of `hash`: a folded node (best or side) or the root, no I/O
    ///
    /// - `None` = final below the root, never folded, or unknown
    /// - an index durable at or past the block: its committed view alone (reads at heights `<=`)
    /// - an index not serving: absent, [`Views::syncing`]
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>> {
        let base = self.graph.at(&self.chain, self.root, hash)?;
        Some(at(self.served.params, &self.durable, &self.serving, base))
    }

    /// Each enabled index's durable tip (syncing ones included), subscribe order
    pub fn durable(&self) -> impl Iterator<Item = (IndexKind, Option<BlockRef>)> + '_ {
        self.durable.iter().map(|(kind, view)| (kind, view.tip()))
    }
}

impl<V> Indexed<V> {
    /// Chain `served` was judged under (the global snapshot judges under the chain view's)
    pub fn chain(&self) -> &Arc<VerifiedChain> {
        &self.chain
    }

    pub fn served(&self) -> &At<V> {
        &self.served
    }

    /// `hash` = a node folded in this snapshot (the root excluded)
    pub fn folded(&self, hash: &BlockHash) -> bool {
        self.graph.contains(hash)
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<BlockRef> {
        self.root
    }
}

fn at<V: View>(
    params: ChainParams,
    durable: &PerIndex<V>,
    serving: &[IndexKind],
    base: Base<Folded>,
) -> At<V> {
    let empty = PerIndex::default();
    let layers = base.folded.as_deref().map_or(&empty, |folded| &folded.layers);
    let views = Views::at(durable, layers, serving, Some(base.at.height));
    At { block: base.at, branch: base.branch, params, views }
}

/// Every enabled index's committed view + a layer above it for each one served
///
/// - [`At`]'s state, and a fold's parent
/// - an index without a layer: not serving (syncing on the final path), read as absent
#[derive(Clone)]
pub struct Views<V> {
    durable: PerIndex<V>,
    layers: PerIndex<Layer>,
}

impl<V: View> Views<V> {
    /// Panics: a layer of no enabled index, or off its `durable`'s chain (`Layer::rebase`)
    pub(crate) fn new(durable: &PerIndex<V>, layers: &PerIndex<Layer>) -> Self {
        let mut rebased = PerIndex::default();
        for (kind, layer) in layers.iter() {
            let view = durable.get(kind).unwrap_or_else(|| panic!("{}: disabled", kind.name()));
            rebased.insert(kind, layer.rebase(view));
        }
        Self { durable: durable.clone(), layers: rebased }
    }

    /// As of the block at `height` (`None` = below genesis): each `serving` index's layer of
    /// `layers`, empty when durable at or past `height` (a layer rebases only onto its own blocks)
    ///
    /// - panics: a serving index durable below `height` with no layer in `layers`
    pub(crate) fn at(
        durable: &PerIndex<V>,
        layers: &PerIndex<Layer>,
        serving: &[IndexKind],
        height: Option<Height>,
    ) -> Self {
        let mut chosen = PerIndex::default();
        for &kind in serving {
            let view = durable.get(kind).unwrap_or_else(|| panic!("{}: disabled", kind.name()));
            let layer = match view.tip().map(|tip| tip.height) >= height {
                true => Layer::empty(view.schema()),
                false => layers.get(kind).cloned().unwrap_or_else(|| {
                    panic!("{}: durable below {height:?}, unfolded there", kind.name())
                }),
            };
            chosen.insert(kind, layer);
        }
        Self::new(durable, &chosen)
    }

    /// Panics: `kind` not served
    pub(crate) fn layer(&self, kind: IndexKind) -> &Layer {
        self.layers.get(kind).unwrap_or_else(|| panic!("{}: not served", kind.name()))
    }

    pub(crate) fn view(&self, kind: IndexKind) -> Option<LayeredView<V>> {
        let layer = self.layers.get(kind)?.clone();
        Some(LayeredView::new(self.durable.get(kind)?.clone(), layer))
    }
}

impl<V> Views<V> {
    /// `kind` enabled, not served yet: catching up alone (its routes: `UNAVAILABLE`)
    pub fn syncing(&self, kind: IndexKind) -> bool {
        self.durable.get(kind).is_some() && self.layers.get(kind).is_none()
    }
}

impl<V: SequenceRead> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<LayeredView<V>>> {
        let view = self.view(IndexKind::CompactBlock)?;
        Some(CompactBlockReader::new(view))
    }

    pub fn tree_state(&self) -> Option<TreeStateReader<LayeredView<V>>> {
        Some(TreeStateReader::new(self.view(IndexKind::TreeState)?))
    }
}

impl<V: MapRead> Views<V> {
    pub fn block_hash(&self) -> Option<BlockHashReader<LayeredView<V>>> {
        Some(BlockHashReader::new(self.view(IndexKind::BlockHash)?))
    }

    pub fn transparent_address(&self) -> Option<TransparentAddressReader<LayeredView<V>>> {
        let view = self.view(IndexKind::TransparentAddress)?;
        Some(TransparentAddressReader::new(view))
    }

    /// Fold parent only (no route reads value-balance)
    pub(crate) fn value_balance(&self) -> Option<ValueBalanceReader<LayeredView<V>>> {
        Some(ValueBalanceReader::new(self.view(IndexKind::ValueBalance)?))
    }
}

/// Latest [`Indexed`] (`None` = nothing folded or committed yet), swapped whole per publish
pub type Published<V> = watch::Receiver<Option<Arc<Indexed<V>>>>;

/// No driver publishes into it (consumers' tests)
#[cfg(any(test, feature = "testing"))]
impl<V: View> Indexed<V> {
    /// At `tip` on `chain`, `tip` = the root: each enabled index serving, = its committed view
    pub fn fixed(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let (mut views, mut serving) = (PerIndex::default(), Vec::new());
        for (kind, view) in durable {
            serving.push(kind);
            views.insert(kind, view);
        }
        Self::new(chain, tip, Some(tip), params, views, serving, Graph::new())
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
