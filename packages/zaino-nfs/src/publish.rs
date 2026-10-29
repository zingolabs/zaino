//! [`NfsPublication`]: one served tip across every enabled index, `GrpcSnapshot`'s index half
//! (`usage.md`)
//!
//! - Pinned for a request's life: nodes through `Arc`, disk through the durable view
//! - Layers rebased onto the durable views they pair with, at build
//! - [`NfsPublication::at`]: any folded node or the root, side branches included (N7)

use std::sync::Arc;

use crate::per_index::PerIndex;
use tokio::sync::watch;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_persistence::{Overlay, OverlayView, View};
use zaino_primitives::protocol::pool_activations;
use zaino_primitives::types::{Block, BlockRef, PerPool};
use zaino_sync::IndexKind;
use zcash_protocol::consensus::{BlockHeight, NetworkType};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::BlockHash;

use crate::fold::Folded;
#[cfg(any(test, feature = "testing"))]
use crate::graph::Node;
use crate::graph::{Base, Graph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    pub network: NetworkType,
    pub schedule: LocalNetwork,
}

impl ChainParams {
    /// Each shielded pool's first height
    pub fn activations(&self) -> PerPool<Option<BlockHeight>> {
        pool_activations(&self.schedule)
    }
}

#[cfg(any(test, feature = "testing"))]
impl ChainParams {
    /// `chain`'s schedule under its network label
    pub fn of(chain: &zaino_source::testing::MockChain) -> Self {
        let schedule = chain.schedule();
        Self { network: schedule.network, schedule: schedule.upgrades }
    }
}

/// Every enabled index as of one block: what a route reads
///
/// - `on_best` = `block` on the publication chain's best branch
pub struct IndexesAtBlock<V> {
    block: BlockRef,
    on_best: bool,
    params: ChainParams,
    views: Views<V>,
}

impl<V: View> IndexesAtBlock<V> {
    /// Highest height `kind` answers: its durable tip when past a best block (finalized), else the
    /// block (side branch: durable data past it = best chain, not this branch)
    pub fn answers_through(&self, kind: IndexKind) -> BlockHeight {
        let durable = self.views.indexes.get(kind).and_then(|(view, _)| view.tip());
        let durable = durable.map(|tip| tip.height);
        match (self.on_best, durable) {
            (true, Some(durable)) => durable.max(self.block.height),
            _ => self.block.height,
        }
    }
}

impl<V> IndexesAtBlock<V> {
    /// `GetLatestBlock` + every read naming no height (one tip across indexes)
    pub fn tip(&self) -> BlockRef {
        self.block
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
/// - `served` = deepest folded best block, else `root` (lowest durable tip of every index)
/// - `durable` = each enabled index's durable view as of this publish (republished per commit)
pub struct NfsPublication<V> {
    chain: Arc<HeaderChainSnapshot>,
    root: Option<BlockRef>,
    served: IndexesAtBlock<V>,
    durable: PerIndex<V>,
    graph: Graph<Folded>,
}

impl<V: View> NfsPublication<V> {
    /// Panics: `tip` neither a node of `graph` nor `root`
    pub(crate) fn new(
        chain: Arc<HeaderChainSnapshot>,
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

    /// Every enabled index as of `hash`: a folded node (best or side) or the root, no I/O
    ///
    /// - `None` = finalized below the root, never folded, or unknown
    /// - an index durable at or past the block: its durable view alone (reads at heights `<=`)
    pub fn at(&self, hash: &BlockHash) -> Option<IndexesAtBlock<V>> {
        let base = self.graph.at(&self.chain, self.root, hash)?;
        Some(at(self.served.params, &self.durable, base))
    }

    /// Each enabled index's durable tip, [`IndexKind::ALL`] order
    pub fn durable(&self) -> impl Iterator<Item = (IndexKind, Option<BlockRef>)> + '_ {
        self.durable.iter().map(|(kind, view)| (kind, view.tip()))
    }
}

impl<V> NfsPublication<V> {
    pub fn served(&self) -> &IndexesAtBlock<V> {
        &self.served
    }

    /// Blocks `served` extends above the root, served first (`served` = the root → none)
    pub fn served_above_durable(&self) -> impl Iterator<Item = &Block> + '_ {
        let served = self.graph.get(&self.served.block.hash);
        std::iter::successors(served, |node| self.graph.get(&node.parent))
            .map(|node| node.block.as_ref())
    }

    #[cfg(test)]
    pub(crate) fn chain(&self) -> &Arc<HeaderChainSnapshot> {
        &self.chain
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<BlockRef> {
        self.root
    }

    /// `hash` = a node folded in this publication (the root excluded)
    #[cfg(test)]
    pub(crate) fn folded(&self, hash: &BlockHash) -> bool {
        self.graph.contains(hash)
    }
}

fn at<V: View>(
    params: ChainParams,
    durable: &PerIndex<V>,
    base: Base<Folded>,
) -> IndexesAtBlock<V> {
    let empty = PerIndex::default();
    let layers = base.folded.as_deref().map_or(&empty, |folded| &folded.layers);
    let views = Views::at(durable, layers, Some(base.at.height));
    IndexesAtBlock { block: base.at, on_best: base.on_best, params, views }
}

/// Every enabled index's durable view + a layer above it: [`IndexesAtBlock`]'s state, and a fold's
/// parent (per-index readers: `indexes.rs`)
#[derive(Clone)]
pub struct Views<V> {
    indexes: PerIndex<(V, Overlay)>,
}

impl<V: View> Views<V> {
    /// As of the block at `height` (`None` = below genesis), per enabled index: an empty layer
    /// when durable at or past `height` (its durable view), else its layer of `layers` rebased
    ///
    /// - panics: an index durable below `height` unfolded there, or its layer off its durable
    ///   view's chain (`Overlay::rebase`)
    pub(crate) fn at(
        durable: &PerIndex<V>,
        layers: &PerIndex<Overlay>,
        height: Option<BlockHeight>,
    ) -> Self {
        let layer = |kind: IndexKind, view: &V| match view.tip().map(|tip| tip.height) >= height {
            true => Overlay::empty(view.schema()),
            false => layers.get(kind).map(|layer| layer.rebase(view)).unwrap_or_else(|| {
                panic!("{}: durable below {height:?}, unfolded there", kind.name())
            }),
        };
        let indexes = durable.iter().map(|(kind, view)| (kind, (view.clone(), layer(kind, view))));
        Self { indexes: indexes.collect() }
    }

    /// Each enabled index: its layer + its reader through it, fold order
    pub(crate) fn each(&self) -> impl Iterator<Item = (IndexKind, &Overlay, OverlayView<V>)> {
        let each = self.indexes.iter();
        each.map(|(kind, (view, layer))| {
            (kind, layer, OverlayView::new(view.clone(), layer.clone()))
        })
    }

    pub(crate) fn view(&self, kind: IndexKind) -> Option<OverlayView<V>> {
        let (view, layer) = self.indexes.get(kind)?;
        Some(OverlayView::new(view.clone(), layer.clone()))
    }
}

/// Latest [`NfsPublication`] (`None` = nothing folded or durable yet), swapped whole per publish
pub type Published<V> = watch::Receiver<Option<Arc<NfsPublication<V>>>>;

/// No driver publishes into it (consumers' tests)
#[cfg(any(test, feature = "testing"))]
impl<V: View> NfsPublication<V> {
    /// At `tip` on `chain`, `tip` = the root: each enabled index = its durable view
    pub fn fixed(
        chain: Arc<HeaderChainSnapshot>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        Self::fixed_above(chain, tip, &[], params, durable)
    }

    /// [`fixed`](Self::fixed) at `root`, then `above_root` (root's child first) folded with no
    /// layers: served = its last
    pub fn fixed_above(
        chain: Arc<HeaderChainSnapshot>,
        root: BlockRef,
        above_root: &[Arc<Block>],
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let views = durable.into_iter().collect();
        let mut graph = Graph::new();
        for block in above_root {
            let (at, parent) = (block.block_ref(), block.header().prev_hash);
            let folded = Arc::new(Folded { layers: PerIndex::default() });
            graph.insert(Node { at, parent, block: Arc::clone(block), folded });
        }
        let tip = above_root.last().map_or(root, |block| block.block_ref());
        Self::new(chain, tip, Some(root), params, views, graph)
    }
}
