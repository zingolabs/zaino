//! [`Indexed`]: one served tip across every enabled index, the global snapshot's index half
//! (`usage.md`)
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
use zaino_index_utxo_set::UtxoSetReader;
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_persistence::PerIndex;
use zaino_persistence::{IndexKind, MapRead, Overlay, OverlayView, SequenceRead, View};
use zaino_primitives::types::{Block, BlockRef};
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
    pub fn activations(&self) -> PoolActivations {
        PoolActivations::new(&self.schedule)
    }
}

#[cfg(any(test, feature = "testing"))]
impl ChainParams {
    /// `chain`'s schedule under its network label
    pub fn of(chain: &zaino_primitives::testing::MockChain) -> Self {
        let schedule = chain.schedule();
        Self { network: schedule.network, schedule: schedule.upgrades }
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

impl<V: View> At<V> {
    /// Highest height `kind` answers: its durable tip when past a best block (final), else the
    /// block (side branch: durable data past it = best chain, not this branch)
    pub fn answers_through(&self, kind: IndexKind) -> BlockHeight {
        let durable = self.views.durable.get(kind).and_then(View::tip).map(|tip| tip.height);
        match self.branch {
            Branch::Best => {
                durable.map_or(self.block.height, |durable| durable.max(self.block.height))
            }
            Branch::Side { .. } => self.block.height,
        }
    }
}

impl<V> At<V> {
    /// `GetLatestBlock` + every read naming no height (one tip across indexes)
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
/// - `served` = deepest folded best block, else `root` (lowest durable tip of every index)
/// - `durable` = each enabled index's committed view as of this publish (republished per commit)
pub struct Indexed<V> {
    chain: Arc<VerifiedChain>,
    root: Option<BlockRef>,
    served: At<V>,
    durable: PerIndex<V>,
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
        graph: Graph<Folded>,
    ) -> Self {
        let base = graph.at(&chain, root, &tip.hash).expect("served tip = a node or the root");
        let served = at(params, &durable, base);
        Self { chain, root, served, durable, graph }
    }

    /// Every enabled index as of `hash`: a folded node (best or side) or the root, no I/O
    ///
    /// - `None` = final below the root, never folded, or unknown
    /// - an index durable at or past the block: its committed view alone (reads at heights `<=`)
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>> {
        let base = self.graph.at(&self.chain, self.root, hash)?;
        Some(at(self.served.params, &self.durable, base))
    }

    /// Each enabled index's durable tip, add order
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

    /// Blocks `served` extends above the root, served first (`served` = the root → none)
    pub fn served_nonfinalized(&self) -> impl Iterator<Item = &Block> + '_ {
        let served = self.graph.get(&self.served.block.hash);
        std::iter::successors(served, |node| self.graph.get(&node.parent))
            .map(|node| node.block.as_ref())
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<BlockRef> {
        self.root
    }
}

fn at<V: View>(params: ChainParams, durable: &PerIndex<V>, base: Base<Folded>) -> At<V> {
    let empty = PerIndex::default();
    let layers = base.folded.as_deref().map_or(&empty, |folded| &folded.layers);
    let views = Views::at(durable, layers, Some(base.at.height));
    At { block: base.at, branch: base.branch, params, views }
}

/// Every enabled index's committed view + a layer above it: [`At`]'s state, and a fold's parent
#[derive(Clone)]
pub struct Views<V> {
    durable: PerIndex<V>,
    layers: PerIndex<Overlay>,
}

impl<V: View> Views<V> {
    /// Panics: a layer of no enabled index, or off its `durable`'s chain (`Overlay::rebase`)
    pub(crate) fn new(durable: &PerIndex<V>, layers: &PerIndex<Overlay>) -> Self {
        let mut rebased = PerIndex::default();
        for (kind, layer) in layers.iter() {
            let view = durable.get(kind).unwrap_or_else(|| panic!("{}: disabled", kind.name()));
            rebased.insert(kind, layer.rebase(view));
        }
        Self { durable: durable.clone(), layers: rebased }
    }

    /// As of the block at `height` (`None` = below genesis), per enabled index: an empty layer
    /// when durable at or past `height` (its committed view), else its layer of `layers`
    ///
    /// - panics: an index durable below `height` unfolded there
    pub(crate) fn at(
        durable: &PerIndex<V>,
        layers: &PerIndex<Overlay>,
        height: Option<BlockHeight>,
    ) -> Self {
        let mut chosen = PerIndex::default();
        for (kind, view) in durable.iter() {
            let layer = match view.tip().map(|tip| tip.height) >= height {
                true => Overlay::empty(view.schema()),
                false => layers.get(kind).cloned().unwrap_or_else(|| {
                    panic!("{}: durable below {height:?}, unfolded there", kind.name())
                }),
            };
            chosen.insert(kind, layer);
        }
        Self::new(durable, &chosen)
    }

    /// Panics: `kind` disabled
    pub(crate) fn layer(&self, kind: IndexKind) -> &Overlay {
        self.layers.get(kind).unwrap_or_else(|| panic!("{}: disabled", kind.name()))
    }

    pub(crate) fn view(&self, kind: IndexKind) -> Option<OverlayView<V>> {
        let layer = self.layers.get(kind)?.clone();
        Some(OverlayView::new(self.durable.get(kind)?.clone(), layer))
    }
}

impl<V: SequenceRead> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<OverlayView<V>>> {
        let view = self.view(IndexKind::CompactBlock)?;
        Some(CompactBlockReader::new(view))
    }

    pub fn tree_state(&self) -> Option<TreeStateReader<OverlayView<V>>> {
        Some(TreeStateReader::new(self.view(IndexKind::TreeState)?))
    }
}

impl<V: MapRead> Views<V> {
    pub fn block_hash(&self) -> Option<BlockHashReader<OverlayView<V>>> {
        Some(BlockHashReader::new(self.view(IndexKind::BlockHash)?))
    }

    pub fn transparent_address(&self) -> Option<TransparentAddressReader<OverlayView<V>>> {
        let view = self.view(IndexKind::TransparentAddress)?;
        Some(TransparentAddressReader::new(view))
    }
}

impl<V: SequenceRead + MapRead> Views<V> {
    /// Fold parents + a peer quorum's prevouts (no route reads utxo-set)
    pub fn utxo_set(&self) -> Option<UtxoSetReader<OverlayView<V>>> {
        Some(UtxoSetReader::new(self.view(IndexKind::UtxoSet)?))
    }
}

/// Latest [`Indexed`] (`None` = nothing folded or committed yet), swapped whole per publish
pub type Published<V> = watch::Receiver<Option<Arc<Indexed<V>>>>;

/// No driver publishes into it (consumers' tests)
#[cfg(any(test, feature = "testing"))]
impl<V: View> Indexed<V> {
    /// At `tip` on `chain`, `tip` = the root: each enabled index = its committed view
    pub fn fixed(
        chain: Arc<VerifiedChain>,
        tip: BlockRef,
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        Self::fixed_above(chain, tip, &[], params, durable)
    }

    /// [`fixed`](Self::fixed) at `root`, then `nonfinalized` (root's child first) folded with no
    /// layers: served = its last
    pub fn fixed_above(
        chain: Arc<VerifiedChain>,
        root: BlockRef,
        nonfinalized: &[Arc<Block>],
        params: ChainParams,
        durable: impl IntoIterator<Item = (IndexKind, V)>,
    ) -> Self {
        let mut views = PerIndex::default();
        for (kind, view) in durable {
            views.insert(kind, view);
        }
        let mut graph = Graph::new();
        for block in nonfinalized {
            let (at, parent) = (block.at(), block.header().prev_hash);
            let folded = Arc::new(Folded { layers: PerIndex::default() });
            graph.insert(Node { at, parent, block: Arc::clone(block), folded });
        }
        let tip = nonfinalized.last().map_or(root, |block| block.at());
        Self::new(chain, tip, Some(root), params, views, graph)
    }
}
