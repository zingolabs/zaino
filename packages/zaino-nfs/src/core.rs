//! [`NfsCore`]: the [`VerifiedChain`] + bodies + folds + each index's durable tip → the served tip
//! and the fetches and folds feeding it (`nfs.md`)
//!
//! - Pure: no I/O, no clock
//! - Root = lowest durable tip of every index; nodes only above it
//! - Active = root within `window` of best: folds every index above its own durable tip; else
//!   (bulk sync) folds nothing, the final path does the work
//! - Node at `h` folds each index durable below `h`
//! - Served tip = deepest folded best block, else the root (reorg → fork point at once)

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};
use zaino_sync::Checked;

use crate::graph::{on_best, Graph, Node};

/// - `wanted` = heights fetching (each one [`Output::Fetch`] until its body or an `Abandon`)
/// - `folding` = block + the indexes its fold covers, per fold in flight
pub(crate) struct NfsCore<F> {
    lookahead: usize,
    window: u32,
    chain: Option<Arc<VerifiedChain>>,
    durable: Vec<Option<BlockRef>>,
    graph: Graph<F>,
    ready: BTreeMap<Height, Arc<Block>>,
    folding: HashMap<BlockHash, (Arc<Block>, Indexes)>,
    wanted: BTreeMap<Height, BlockHash>,
    shown: Option<Shown>,
}

/// Last publish; any part moved = republished
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    tip: BlockRef,
    durable: Vec<Option<BlockRef>>,
}

/// Index positions (`Input::Durable` order), at most 32
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Indexes(u32);

impl Indexes {
    pub(crate) fn one(position: usize) -> Self {
        assert!(position < 32, "at most 32 indexes");
        Self(1 << position)
    }

    pub(crate) fn contains(self, position: usize) -> bool {
        position < 32 && self.0 & (1 << position) != 0
    }

    pub(crate) fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// - `Body` = a checked answer to a `Fetch` (stale = ignored)
/// - `Folded.covers` = the `Fold`'s (a stale result ignored)
/// - `Durable` = every index's durable tip, after each commit
#[derive(Debug, Clone)]
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body(Checked),
    Folded { at: BlockRef, covers: Indexes, folded: Arc<F> },
    Durable(Vec<Option<BlockRef>>),
}

/// - `Fold.parent` = `None`: fold on the committed stores at the root
/// - `Publish(None)` = nothing servable (nothing durable, or the root off best): the last
///   snapshot withdrawn
#[derive(Debug, Clone)]
pub(crate) enum Output<F> {
    Fetch { at: BlockRef, record: Record },
    Abandon(BlockRef),
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block>, covers: Indexes },
    Publish(Option<SnapshotTip<F>>),
}

/// `tip` = a node of `graph`, or `root` (read from the committed stores alone)
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTip<F> {
    pub(crate) chain: Arc<VerifiedChain>,
    pub(crate) tip: BlockRef,
    pub(crate) root: Option<BlockRef>,
    pub(crate) graph: Graph<F>,
}

fn height(tip: Option<BlockRef>) -> Option<Height> {
    tip.map(|tip| tip.height)
}

impl<F> NfsCore<F> {
    /// - `count` = enabled indexes, each durable at nothing until the first `Input::Durable`
    /// - `lookahead` = bodies fetched or folding ahead of the next fold
    /// - `window` = most blocks the root may trail best and still fold (`2 · depth`)
    pub(crate) fn new(lookahead: usize, count: usize, window: u32) -> Self {
        assert!(lookahead > 0, "at least one block in flight");
        assert!((1..=32).contains(&count), "1 to 32 indexes");
        Self {
            lookahead,
            window,
            chain: None,
            durable: vec![None; count],
            graph: Graph::new(),
            ready: BTreeMap::new(),
            folding: HashMap::new(),
            wanted: BTreeMap::new(),
            shown: None,
        }
    }

    pub(crate) fn step(&mut self, input: Input<F>) -> Vec<Output<F>> {
        let mut out = Vec::new();
        match input {
            Input::Chain(chain) => self.chain = Some(chain),
            Input::Body(body) => self.body(body),
            Input::Folded { at, covers, folded } => self.folded(at, covers, folded),
            Input::Durable(tips) => self.durable(tips),
        }
        let Some(chain) = self.chain.clone() else { return out };
        if !self.active(&chain) {
            self.graph = Graph::new();
            self.folding.clear();
        }
        self.graph.prune(&chain, self.root());
        self.forget(&chain, &mut out);
        self.fold(&chain, &mut out);
        self.publish(&chain, &mut out);
        self.want(&chain, &mut out);
        out
    }

    /// A wanted body → ready; any other (abandoned while in flight) dropped
    fn body(&mut self, body: Checked) {
        let at = body.at();
        if self.wanted.get(&at.height) == Some(&at.hash) {
            self.wanted.remove(&at.height);
            self.ready.insert(at.height, Arc::clone(body.block()));
        }
    }

    /// Lowest durable tip of every index (`None` = one holds nothing)
    fn root(&self) -> Option<BlockRef> {
        self.durable.iter().copied().min_by_key(|tip| height(*tip)).flatten()
    }

    fn next_root(&self) -> Height {
        self.root().map_or(Height::GENESIS, |root| root.height.next())
    }

    /// Root within `window` of best: the NFS folds (else bulk sync: the final path alone)
    fn active(&self, chain: &VerifiedChain) -> bool {
        let next = u32::from(self.next_root());
        (u32::from(chain.best().height) + 1).saturating_sub(next) <= self.window
    }

    /// Durable tips never move back
    fn durable(&mut self, tips: Vec<Option<BlockRef>>) {
        assert_eq!(tips.len(), self.durable.len(), "one durable tip per enabled index");
        for (index, tip) in tips.into_iter().enumerate() {
            let back = height(tip) < height(self.durable[index]);
            assert!(!back, "N3: index {index}'s durable tip never moves back");
            self.durable[index] = tip;
        }
    }

    /// Kept iff its fold is the one in flight, above the root, on a held parent
    fn folded(&mut self, at: BlockRef, covers: Indexes, folded: Arc<F>) {
        if self.folding.get(&at.hash).is_none_or(|(_, folding)| *folding != covers) {
            return;
        }
        let (block, _) = self.folding.remove(&at.hash).expect("in flight");
        let parent = block.header().prev_hash;
        let root = self.root();
        if Some(at.height) > height(root) && self.graph.holds_parent(root, parent, at.height) {
            self.graph.insert(Node { at, parent, block, folded });
        }
    }

    /// Bodies and wants no longer needed: inactive, at or below the root, off best, or folded
    fn forget(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let (next, active, graph) = (self.next_root(), self.active(chain), &self.graph);
        let needed = |at: BlockRef| {
            active && at.height >= next && on_best(chain, at) && !graph.contains(&at.hash)
        };
        self.ready.retain(|height, block| {
            needed(BlockRef { hash: block.header().hash, height: *height })
        });
        self.wanted.retain(|height, hash| {
            let at = BlockRef { hash: *hash, height: *height };
            let keep = needed(at);
            if !keep {
                out.push(Output::Abandon(at));
            }
            keep
        });
    }

    /// Next best block above the folded run from the root, once its body is here (one at a time)
    fn fold(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        if !self.active(chain) || !self.folding.is_empty() {
            return;
        }
        let top = self.graph.best_top(chain, self.root());
        let at_height = top.map_or(self.next_root(), |top| top.at.height.next());
        let Some(hash) = chain.hash_at(at_height) else { return };
        let Some(block) = self.ready.remove(&at_height) else { return };
        let below = |index: &usize| height(self.durable[*index]) < Some(at_height);
        let covers = (0..self.durable.len()).filter(below).map(Indexes::one);
        let covers = covers.fold(Indexes::default(), Indexes::union);
        let parent = top.map(|top| Arc::clone(&top.folded));
        let at = BlockRef { hash, height: at_height };
        out.push(Output::Fold { at, parent, block: Arc::clone(&block), covers });
        self.folding.insert(hash, (block, covers));
    }

    /// Deepest folded best block, else the root; `None` = nothing durable, or the root off the
    /// best chain (a lost header store)
    fn served(&self, chain: &VerifiedChain) -> Option<Shown> {
        let root = self.root();
        let top = self.graph.best_top(chain, root).map(|top| top.at);
        let tip = top.or(root).filter(|tip| on_best(chain, *tip))?;
        Some(Shown { tip, durable: self.durable.clone() })
    }

    /// Served tip or a durable tip moved → published; nothing servable after a publish → withdrawn
    fn publish(&mut self, chain: &Arc<VerifiedChain>, out: &mut Vec<Output<F>>) {
        let Some(shown) = self.served(chain) else {
            if self.shown.take().is_some() {
                out.push(Output::Publish(None));
            }
            return;
        };
        if self.shown.as_ref() == Some(&shown) {
            return;
        }
        let tip = shown.tip;
        self.shown = Some(shown);
        let (chain, root, graph) = (Arc::clone(chain), self.root(), self.graph.clone());
        out.push(Output::Publish(Some(SnapshotTip { chain, tip, root, graph })));
    }

    /// First `lookahead` best heights above the root with no node: each held, folding or wanted
    /// (a new want = one `Fetch`)
    fn want(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        if !self.active(chain) {
            return;
        }
        let heights = self.next_root().up_to(chain.best().height).map(|height| BlockRef {
            hash: chain.hash_at(height).expect("at or below the best tip"),
            height,
        });
        let unfolded: Vec<BlockRef> =
            heights.filter(|at| !self.graph.contains(&at.hash)).take(self.lookahead).collect();
        for at in unfolded {
            let held = self.ready.contains_key(&at.height) || self.folding.contains_key(&at.hash);
            if held || self.wanted.contains_key(&at.height) {
                continue;
            }
            self.wanted.insert(at.height, at.hash);
            let record = chain.header_at(at.height).expect("a wanted height is best");
            out.push(Output::Fetch { at, record });
        }
    }

    /// N1–N4 and the fetch bookkeeping; panics naming the invariant broken
    pub(crate) fn check(&self) {
        let Some(chain) = &self.chain else {
            let empty = self.graph.is_empty() && self.ready.is_empty() && self.folding.is_empty();
            let empty = empty && self.wanted.is_empty();
            assert!(empty && self.shown.is_none(), "nothing before a chain");
            return;
        };
        if !self.active(chain) {
            let idle = self.graph.is_empty() && self.ready.is_empty() && self.folding.is_empty();
            assert!(idle && self.wanted.is_empty(), "N5: nothing folded outside the window");
        }
        self.graph.check(chain, self.root());
        let served = self.served(chain).map(|shown| shown.tip);
        let shown = self.shown.as_ref().map(|shown| shown.tip);
        assert_eq!(shown, served, "N4: served tip = the deepest folded best block");
        let next = self.next_root();
        for (height, block) in &self.ready {
            let at = BlockRef { hash: block.header().hash, height: *height };
            assert!(*height >= next, "fetch: ready bodies above the root");
            assert!(on_best(chain, at), "N1: every ready body is best");
            assert!(!self.graph.contains(&at.hash), "fetch: a folded block is not ready");
            assert!(!self.wanted.contains_key(height), "fetch: a ready body is not wanted");
        }
        for hash in self.folding.keys() {
            assert!(!self.graph.contains(hash), "fold: nothing folds twice");
        }
        for (height, hash) in &self.wanted {
            assert!(*height >= next, "fetch: wants above the root");
            assert_eq!(chain.hash_at(*height), Some(*hash), "N1: every want is best");
            assert!(!self.graph.contains(hash), "fetch: a folded block is not wanted");
        }
    }
}

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model;
