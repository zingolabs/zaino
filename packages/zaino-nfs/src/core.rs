//! [`NfsCore`]: the [`VerifiedChain`] + bodies + folds + each index's state → the served tip and
//! the fetches and folds feeding it (`nfs.md`)
//!
//! - Pure: no I/O, no clock
//! - Root = lowest durable tip of the serving indexes; nodes only above it
//! - Node at `h` folds each serving index durable below `h`
//! - Serving set changed → every node refolded from its block (published tip held meanwhile)
//! - Served tip = deepest folded best block, else the root (reorg → fork point at once)

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};
use zaino_sync::Checked;

use crate::graph::{on_best, Graph, Node};

/// - `wanted` = heights fetching (each one [`Output::Fetch`] until its body or an `Abandon`)
/// - `folding` = block + the indexes its fold covers, per fold in flight
/// - `held` = served tip before a refold: republished only once the refold reaches it again
pub(crate) struct NfsCore<F> {
    lookahead: usize,
    chain: Option<Arc<VerifiedChain>>,
    durable: Vec<Option<BlockRef>>,
    serving: Indexes,
    graph: Graph<F>,
    ready: BTreeMap<Height, Arc<Block>>,
    folding: HashMap<BlockHash, (Arc<Block>, Indexes)>,
    wanted: BTreeMap<Height, BlockHash>,
    shown: Option<Shown>,
    held: Option<BlockRef>,
}

/// Last publish; any part moved = republished
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    tip: BlockRef,
    covers: Indexes,
    serving: Indexes,
    durable: Vec<Option<BlockRef>>,
}

/// Index positions (`Input::Indexes` order), at most 32
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

    /// `other` ⊆ `self`
    pub(crate) fn covers(self, other: Self) -> bool {
        other.0 & !self.0 == 0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = usize> {
        (0..32).filter(move |position| self.contains(*position))
    }
}

/// - `Body` = a checked answer to a `Fetch` (stale = ignored)
/// - `Folded.covers` = the `Fold`'s (a stale result, from before a refold, ignored)
/// - `Indexes` = every index's (durable tip, serving), after each chain change and commit
#[derive(Debug, Clone)]
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body(Checked),
    Folded { at: BlockRef, covers: Indexes, folded: Arc<F> },
    Indexes(Vec<(Option<BlockRef>, bool)>),
}

/// - `Fold.parent` = `None`: fold on the committed stores at the root
/// - `Publish(None)` = nothing servable (none serving, nothing durable): the last snapshot withdrawn
#[derive(Debug, Clone)]
pub(crate) enum Output<F> {
    Fetch { at: BlockRef, record: Record },
    Abandon(BlockRef),
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block>, covers: Indexes },
    Publish(Option<SnapshotTip<F>>),
}

/// - `tip` = a node of `graph`, or `root` (read from the committed stores alone)
/// - `serving` = the indexes snapshots serve
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTip<F> {
    pub(crate) chain: Arc<VerifiedChain>,
    pub(crate) tip: BlockRef,
    pub(crate) root: Option<BlockRef>,
    pub(crate) graph: Graph<F>,
    pub(crate) serving: Indexes,
}

fn height(tip: Option<BlockRef>) -> Option<Height> {
    tip.map(|tip| tip.height)
}

impl<F> NfsCore<F> {
    /// - `count` = enabled indexes, none serving until the first `Input::Indexes`
    /// - `lookahead` = bodies fetched or folding ahead of the next fold
    pub(crate) fn new(lookahead: usize, count: usize) -> Self {
        assert!(lookahead > 0, "at least one block in flight");
        assert!((1..=32).contains(&count), "1 to 32 indexes");
        Self {
            lookahead,
            chain: None,
            durable: vec![None; count],
            serving: Indexes::default(),
            graph: Graph::new(),
            ready: BTreeMap::new(),
            folding: HashMap::new(),
            wanted: BTreeMap::new(),
            shown: None,
            held: None,
        }
    }

    pub(crate) fn step(&mut self, input: Input<F>) -> Vec<Output<F>> {
        let mut out = Vec::new();
        match input {
            Input::Chain(chain) => self.chain = Some(chain),
            Input::Body(body) => self.body(body),
            Input::Folded { at, covers, folded } => self.folded(at, covers, folded),
            Input::Indexes(states) => self.indexes(states),
        }
        let Some(chain) = self.chain.clone() else { return out };
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

    /// Lowest durable tip of the serving indexes (none serving: of every index)
    fn root(&self) -> Option<BlockRef> {
        let every =
            (0..self.durable.len()).map(Indexes::one).fold(Indexes::default(), Indexes::union);
        let from = if self.serving.is_empty() { every } else { self.serving };
        from.iter().map(|index| self.durable[index]).min_by_key(|tip| height(*tip)).flatten()
    }

    fn next_root(&self) -> Height {
        self.root().map_or(Height::GENESIS, |root| root.height.next())
    }

    /// - Durable tips never move back
    /// - Serving set changed → refold: every best node's block back to `ready`, the graph and
    ///   folds in flight dropped, the served tip held until the refold reaches it
    fn indexes(&mut self, states: Vec<(Option<BlockRef>, bool)>) {
        assert_eq!(states.len(), self.durable.len(), "one state per enabled index");
        let mut serving = Indexes::default();
        for (index, (tip, serves)) in states.into_iter().enumerate() {
            let back = height(tip) < height(self.durable[index]);
            assert!(!back, "N3: index {index}'s durable tip never moves back");
            self.durable[index] = tip;
            if serves {
                serving = serving.union(Indexes::one(index));
            }
        }
        if serving == self.serving {
            return;
        }
        self.serving = serving;
        if let Some(chain) = &self.chain {
            for node in self.graph.nodes().filter(|node| on_best(chain, node.at)) {
                self.ready.insert(node.at.height, Arc::clone(&node.block));
            }
        }
        self.graph = Graph::new();
        self.folding.clear();
        self.held = self.shown.as_ref().map(|shown| shown.tip);
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
            self.graph.insert(Node { at, parent, block, folded, covers });
        }
    }

    /// Bodies and wants no longer needed: none serving, at or below the root, off best, or folded
    fn forget(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let (next, serving, graph) = (self.next_root(), !self.serving.is_empty(), &self.graph);
        let needed = |at: BlockRef| {
            serving && at.height >= next && on_best(chain, at) && !graph.contains(&at.hash)
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
        if self.serving.is_empty() || !self.folding.is_empty() {
            return;
        }
        let root = self.root();
        let top = self.graph.best_top(chain, root);
        let at_height = top.map_or(self.next_root(), |top| top.at.height.next());
        let Some(hash) = chain.hash_at(at_height) else { return };
        let Some(block) = self.ready.remove(&at_height) else { return };
        let below = |index: &usize| height(self.durable[*index]) < Some(at_height);
        let covers = self.serving.iter().filter(below).map(Indexes::one);
        let covers = covers.fold(Indexes::default(), Indexes::union);
        let parent = top.map(|top| Arc::clone(&top.folded));
        let at = BlockRef { hash, height: at_height };
        out.push(Output::Fold { at, parent, block: Arc::clone(&block), covers });
        self.folding.insert(hash, (block, covers));
    }

    /// Deepest folded best block (its indexes), else the root (the serving ones); `None` = the
    /// root off the best chain (a lost header store) or nothing durable
    fn served(&self, chain: &VerifiedChain) -> Option<Shown> {
        let root = self.root();
        let top = self.graph.best_top(chain, root);
        let covers = top.map_or(self.serving, |top| top.covers);
        let tip = top.map(|top| top.at).or(root).filter(|tip| on_best(chain, *tip))?;
        Some(Shown { tip, covers, serving: self.serving, durable: self.durable.clone() })
    }

    /// - Served tip, its indexes or a durable tip moved → published (held while a refold catches up)
    /// - Nothing servable after a publish → withdrawn
    fn publish(&mut self, chain: &Arc<VerifiedChain>, out: &mut Vec<Output<F>>) {
        let Some(shown) = self.served(chain) else {
            if self.shown.take().is_some() {
                out.push(Output::Publish(None));
            }
            return;
        };
        if let Some(held) = self.held {
            let pending =
                !(self.ready.is_empty() && self.folding.is_empty() && self.wanted.is_empty());
            if on_best(chain, held) && shown.tip.height < held.height && pending {
                return;
            }
            self.held = None;
        }
        if self.shown.as_ref() == Some(&shown) {
            return;
        }
        let (tip, serving) = (shown.tip, self.serving);
        self.shown = Some(shown);
        let (chain, root, graph) = (Arc::clone(chain), self.root(), self.graph.clone());
        out.push(Output::Publish(Some(SnapshotTip { chain, tip, root, graph, serving })));
    }

    /// First `lookahead` best heights above the root with no node: each held, folding or wanted
    /// (a new want = one `Fetch`)
    fn want(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        if self.serving.is_empty() {
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
        self.graph.check(chain, self.root(), self.serving);
        if self.held.is_none() {
            if let Some(served) = self.served(chain) {
                let shown = self.shown.as_ref().map(|shown| shown.tip);
                assert_eq!(
                    shown,
                    Some(served.tip),
                    "N4: served tip = the deepest folded best block"
                );
            }
        }
        let next = self.next_root();
        for (height, block) in &self.ready {
            let at = BlockRef { hash: block.header().hash, height: *height };
            assert!(*height >= next, "fetch: ready bodies above the root");
            assert!(on_best(chain, at), "N1: every ready body is best");
            assert!(!self.graph.contains(&at.hash), "fetch: a folded block is not ready");
            assert!(!self.wanted.contains_key(height), "fetch: a ready body is not wanted");
        }
        for (hash, (_, covers)) in &self.folding {
            assert!(!self.graph.contains(hash), "fold: nothing folds twice");
            assert!(self.serving.covers(*covers), "N2: a fold covers serving indexes only");
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
