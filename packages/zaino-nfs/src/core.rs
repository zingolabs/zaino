//! [`NfsCore`]: the [`VerifiedChain`] + bodies + folds + durable tips → the final stream, the
//! served tip and the fetches and folds feeding them (`nfs.md` §6, §9)
//!
//! - Pure: no I/O, no clock
//! - Fetch (one per want, until checked or abandoned), fold, send = the driver's
//! - Root = lowest durable tip of the joined indexes, nodes only above it
//! - Lagging index (behind the root at boot, or enabled late): out of folds and snapshots, fed by
//!   the stream from its own tip; joins once durable at the root (`nfs.md` §6 "Late indexes")
//! - Final + no folded parent → sent unfolded (writers fold)
//! - Else folded parent first as it joins the best → sent folded once final (lockstep finality)
//! - Served tip = deepest folded best block, else the root (reorg → fork point at once)

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};
use zaino_traffic::Urgency;

use crate::fetch::Checked;
use crate::graph::{on_best, Graph, Node};

/// Folded payload `F` = one block's folded state per covered index (`Folded`, toy in the model)
///
/// - `groups` = indexes joining together (value-balance + compact-block: one fee per self-folded
///   step on both sides)
/// - `wanted` = heights fetching (each one [`Output::Fetch`] until its body or an `Abandon`)
/// - `folding` = block + the indexes its fold covers, per fold in flight
/// - `undelivered` = `Send`s not yet `Delivered` (a full queue holds them, never the loop)
pub(crate) struct NfsCore<F> {
    lookahead: usize,
    chain: Option<Arc<VerifiedChain>>,
    durable: Vec<Option<BlockRef>>,
    groups: Vec<Indexes>,
    joined: Indexes,
    sent: Option<Sent>,
    undelivered: usize,
    graph: Graph<F>,
    ready: BTreeMap<Height, Checked>,
    folding: HashMap<BlockHash, (Arc<Block>, Indexes)>,
    wanted: BTreeMap<Height, BlockHash>,
    shown: Shown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sent {
    at: BlockRef,
    folded: bool,
}

/// Last publish (`covers` = the indexes `tip` serves: its node's, the root's = `joined`); any
/// part moved = republished
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    tip: Option<BlockRef>,
    covers: Indexes,
    joined: Indexes,
    durable: Vec<Option<BlockRef>>,
}

/// Index positions (`Input::Durable.index`), at most 32
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Indexes(u32);

impl Indexes {
    /// Positions `0..count`
    pub(crate) fn first(count: usize) -> Self {
        assert!(count <= 32, "at most 32 indexes");
        Self(u32::MAX.checked_shr(32 - count as u32).unwrap_or(0))
    }

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
/// - `Delivered` = the oldest `Send` in every queue
#[derive(Debug, Clone)]
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body(Checked),
    Folded { at: BlockRef, folded: Arc<F> },
    Durable { index: usize, tip: Option<BlockRef> },
    Delivered,
}

/// - `Send`s in list order; the rest in any
/// - `Fetch.urgency` = `Tip` above the final tip, `Bulk` below
/// - `Fold.parent` = `None`: fold on the committed stores at the root
/// - `Fold.covers` = the indexes folded (the joined ones; `parent` folds exactly these)
#[derive(Debug, Clone)]
pub(crate) enum Output<F> {
    Fetch { at: BlockRef, record: Record, urgency: Urgency },
    Abandon(BlockRef),
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block>, covers: Indexes },
    Send(Final<F>),
    Publish(SnapshotTip<F>),
}

/// One final-stream step (`folded` = `None`, or lacking an index: that index's writer folds it)
#[derive(Debug, Clone)]
pub(crate) struct Final<F> {
    pub(crate) block: Arc<Block>,
    pub(crate) folded: Option<Arc<F>>,
}

/// - `tip` = a node of `graph`, or `root` (read from the committed stores alone)
/// - `joined` = the indexes the root serves (a node serves the ones it folded)
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTip<F> {
    pub(crate) chain: Arc<VerifiedChain>,
    pub(crate) tip: BlockRef,
    pub(crate) root: Option<BlockRef>,
    pub(crate) graph: Graph<F>,
    pub(crate) joined: Indexes,
}

/// Index `index`'s durable block off the final chain (resync required)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Diverged {
    pub(crate) index: usize,
    pub(crate) height: Height,
    pub(crate) expected: BlockHash,
    pub(crate) got: BlockHash,
}

fn height(tip: Option<BlockRef>) -> Option<Height> {
    tip.map(|tip| tip.height)
}

impl<F> NfsCore<F> {
    /// - `durable` = each enabled index's durable tip (`Durable.index` = its position)
    /// - `groups` = partition of the positions (each joins whole)
    /// - `lookahead` = bodies fetched or folding ahead of the next one needed, per window (the
    ///   stream's, and the tip's while a lagging index holds the stream below the root)
    /// - `lookahead` = most sends in flight too
    /// - Boot: joined = the groups whose lowest tip is the highest
    /// - Boot: the stream from the lowest tip
    pub(crate) fn new(
        lookahead: usize,
        durable: Vec<Option<BlockRef>>,
        groups: Vec<Indexes>,
    ) -> Self {
        assert!(lookahead > 0, "at least one block in flight");
        assert!(!durable.is_empty(), "an index to feed");
        let mut all = Indexes::default();
        for group in &groups {
            assert!(!group.is_empty() && all.0 & group.0 == 0, "groups partition the indexes");
            all = all.union(*group);
        }
        assert_eq!(all, Indexes::first(durable.len()), "groups partition the indexes");
        let floor =
            |group: &Indexes| group.iter().map(|index| height(durable[index])).min().flatten();
        let root = groups.iter().map(floor).max().flatten();
        let at_root = groups.iter().filter(|group| floor(group) == root);
        let joined = at_root.fold(Indexes::default(), |joined, group| joined.union(*group));
        let lowest = durable.iter().copied().min_by_key(|tip| height(*tip)).flatten();
        Self {
            lookahead,
            chain: None,
            shown: Shown { tip: None, covers: joined, joined, durable: durable.clone() },
            durable,
            groups,
            joined,
            sent: lowest.map(|at| Sent { at, folded: false }),
            undelivered: 0,
            graph: Graph::new(),
            ready: BTreeMap::new(),
            folding: HashMap::new(),
            wanted: BTreeMap::new(),
        }
    }

    /// `Err` = an index's durable block off the final chain (resync required)
    pub(crate) fn step(&mut self, input: Input<F>) -> Result<Vec<Output<F>>, Diverged> {
        let mut out = Vec::new();
        match input {
            Input::Chain(chain) => self.follow(chain)?,
            Input::Body(body) => self.body(body),
            Input::Folded { at, folded } => self.folded(at, folded),
            Input::Durable { index, tip } => self.durable(index, tip),
            Input::Delivered => {
                assert!(self.undelivered > 0, "a delivery for a send in flight");
                self.undelivered -= 1;
            }
        }
        let Some(chain) = self.chain.clone() else { return Ok(out) };
        self.graph.prune(&chain, self.root());
        self.forget(&chain, &mut out);
        if !self.restarting(&chain) {
            self.send(&chain, &mut out);
            self.fold(&chain, &mut out);
            self.publish(&chain, &mut out);
        }
        self.want(&chain, &mut out);
        Ok(out)
    }

    /// A wanted body → ready; any other (abandoned while in flight) dropped
    fn body(&mut self, body: Checked) {
        let at = body.at();
        if self.wanted.get(&at.height) == Some(&at.hash) {
            self.wanted.remove(&at.height);
            self.ready.insert(at.height, body);
        }
    }

    /// Lowest durable tip of the joined indexes (`None` = one holds nothing)
    fn root(&self) -> Option<BlockRef> {
        let joined = self.joined.iter().map(|index| self.durable[index]);
        joined.min_by_key(|tip| height(*tip)).flatten()
    }

    /// Lowest durable tip of every index: where the stream started
    fn lowest(&self) -> Option<BlockRef> {
        self.durable.iter().copied().min_by_key(|tip| height(*tip)).flatten()
    }

    fn next_send(&self) -> Height {
        self.sent.map_or(Height::GENESIS, |sent| sent.at.height.next())
    }

    /// Durable tip above the final tip (lost header store): nothing sent, folded or served
    /// meanwhile
    fn restarting(&self, chain: &VerifiedChain) -> bool {
        let final_height = height(chain.final_tip());
        self.durable.iter().flatten().any(|tip| Some(tip.height) > final_height)
    }

    fn follow(&mut self, chain: Arc<VerifiedChain>) -> Result<(), Diverged> {
        if let Some(seen) = self.chain.as_ref().and_then(|chain| chain.final_tip()) {
            let back = chain.final_tip().is_none_or(|tip| tip.height < seen.height);
            assert!(!back, "H2: the final tip never moves back");
            assert_eq!(
                chain.hash_at(seen.height),
                Some(seen.hash),
                "H2: a final block never changes"
            );
        }
        let final_height = height(chain.final_tip());
        for (index, tip) in self.durable.iter().enumerate() {
            let Some(tip) = tip.filter(|tip| Some(tip.height) <= final_height) else { continue };
            let got = chain.hash_at(tip.height).expect("at or below the final tip");
            if got != tip.hash {
                let (height, expected) = (tip.height, tip.hash);
                return Err(Diverged { index, height, expected, got });
            }
        }
        self.chain = Some(chain);
        Ok(())
    }

    /// - Kept iff above the root on a held parent
    /// - A refold replaces the node it widens
    fn folded(&mut self, at: BlockRef, folded: Arc<F>) {
        let (block, covers) =
            self.folding.remove(&at.hash).expect("a fold result for a fold in flight");
        let parent = block.header().prev_hash;
        let root = self.root();
        if Some(at.height) > height(root) && self.graph.holds_parent(root, parent, at.height) {
            if let Some(held) = self.graph.get(&at.hash) {
                assert!(covers.covers(held.covers), "J4: a refold only widens a node's indexes");
            }
            self.graph.insert(Node { at, parent, block, folded, covers });
        }
    }

    fn durable(&mut self, index: usize, tip: Option<BlockRef>) {
        assert!(index < self.durable.len(), "a durable tip of an enabled index");
        assert!(height(tip) >= height(self.durable[index]), "N3: a durable tip never moves back");
        if tip != self.durable[index] {
            let sent = self.sent.map(|sent| sent.at.height);
            assert!(height(tip) <= sent, "N3: a durable tip is a block the stream sent");
        }
        self.durable[index] = tip;
        let root = self.root();
        let lagging = self.groups.iter().filter(|group| !self.joined.covers(**group));
        let at_root = |group: &&Indexes| group.iter().all(|member| self.durable[member] == root);
        let joining: Vec<Indexes> = lagging.filter(at_root).copied().collect();
        for group in joining {
            self.join(group);
        }
    }

    /// `group` folded and served from now on (the root unmoved: it holds exactly the root)
    fn join(&mut self, group: Indexes) {
        let root = self.root();
        let at_root = group.iter().all(|member| self.durable[member] == root);
        assert!(at_root, "J2: an index joins at the root only");
        self.joined = self.joined.union(group);
    }

    /// Bodies and wants no longer needed: off best, already sent, or folded (each want abandoned)
    fn forget(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let next = self.next_send();
        let graph = &self.graph;
        let needed =
            |at: BlockRef| at.height >= next && on_best(chain, at) && !graph.contains(&at.hash);
        self.ready.retain(|_, body| needed(body.at()));
        self.wanted.retain(|height, hash| {
            let at = BlockRef { hash: *hash, height: *height };
            let keep = needed(at);
            if !keep {
                out.push(Output::Abandon(at));
            }
            keep
        });
    }

    /// Final heights in order: a node's folded, else (stream unfolded so far, no fold in flight)
    /// the body unfolded
    ///
    /// - A node folded without an index: that index folds the block itself (lagging when folded)
    /// - At most `lookahead` undelivered (a lagging index's full queue pauses the stream alone)
    fn send(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        while self.undelivered < self.lookahead {
            let height = self.next_send();
            if Some(height) > chain.final_tip().map(|tip| tip.height) {
                return;
            }
            let hash = chain.hash_at(height).expect("at or below the final tip");
            let folded = match self.graph.get(&hash) {
                Some(node) => {
                    out.push(Output::Send(Final {
                        block: Arc::clone(&node.block),
                        folded: Some(Arc::clone(&node.folded)),
                    }));
                    true
                }
                None => {
                    let stream_folded = self.sent.is_some_and(|sent| sent.folded);
                    if stream_folded || self.folding.contains_key(&hash) {
                        return;
                    }
                    let Some(body) = self.ready.remove(&height) else { return };
                    out.push(Output::Send(Final { block: Arc::clone(body.block()), folded: None }));
                    false
                }
            };
            self.sent = Some(Sent { at: BlockRef { hash, height }, folded });
            self.undelivered += 1;
        }
    }

    /// Next best block above the run folding every joined index, once its parent is folded
    ///
    /// - Never a sent height (an index may hold it)
    /// - On the root: above the final tip, next to send on a folded stream (never unfolded after
    ///   folded), or [`held_below`](Self::held_below)
    /// - A node folded before an index joined: refolded from its own block (wider)
    fn fold(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let root = self.root();
        let top = self.graph.covering_top(chain, root, self.joined);
        let height = top.map_or(self.next_root(), |top| top.at.height.next());
        if height < self.next_send() {
            return;
        }
        let Some(hash) = chain.hash_at(height) else { return };
        if self.folding.contains_key(&hash) {
            return;
        }
        let parent = match top {
            Some(top) => Some(Arc::clone(&top.folded)),
            None => {
                let final_height = chain.final_tip().map(|tip| tip.height);
                let stream_folded = self.sent.is_some_and(|sent| sent.folded);
                let next = height == self.next_send() && stream_folded;
                if Some(height) <= final_height && !next && !self.held_below(chain, height) {
                    return;
                }
                None
            }
        };
        let block = match self.graph.get(&hash) {
            Some(node) => Arc::clone(&node.block),
            None => {
                let Some(body) = self.ready.remove(&height) else { return };
                Arc::clone(body.block())
            }
        };
        if parent.is_none() {
            let lockstep = self.joined.iter().all(|index| self.durable[index] == root);
            assert!(lockstep, "N3: a fold on the root = every joined index durable at it");
        }
        let at = BlockRef { hash, height };
        let covers = self.joined;
        out.push(Output::Fold { at, parent, block: Arc::clone(&block), covers });
        self.folding.insert(hash, (block, covers));
    }

    /// Final `height` foldable on the root: the stream held below it by a lagging index, every
    /// joined index durable at the root, the root within one non-final window (best − final) of
    /// the final tip
    ///
    /// - Restart after downtime: the joined serve the tip meanwhile (nodes ≤ two windows + one per
    ///   block until the lagging one joins)
    /// - Farther behind (bulk): they wait for the stream (no unbounded nodes)
    fn held_below(&self, chain: &VerifiedChain, height: Height) -> bool {
        let root = self.root();
        let lockstep = self.joined.iter().all(|index| self.durable[index] == root);
        let (Some(root), Some(final_tip)) = (root, chain.final_tip()) else { return false };
        let behind = u32::from(final_tip.height).saturating_sub(u32::from(root.height));
        let window = u32::from(chain.best().height).saturating_sub(u32::from(final_tip.height));
        height > self.next_send() && lockstep && behind <= window
    }

    fn next_root(&self) -> Height {
        self.root().map_or(Height::GENESIS, |root| root.height.next())
    }

    /// Served tip, the indexes it serves (a refold widening it), the joined set or a durable tip
    /// moved → published
    fn publish(&mut self, chain: &Arc<VerifiedChain>, out: &mut Vec<Output<F>>) {
        let shown = self.serving(chain);
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        if let Some(tip) = self.shown.tip {
            let (chain, root, graph) = (Arc::clone(chain), self.root(), self.graph.clone());
            let joined = self.joined;
            out.push(Output::Publish(SnapshotTip { chain, tip, root, graph, joined }));
        }
    }

    /// Deepest folded best block (its node's indexes), else the root (the joined)
    fn serving(&self, chain: &VerifiedChain) -> Shown {
        let top = self.graph.best_top(chain, self.root());
        let covers = top.map_or(self.joined, |top| top.covers);
        let tip = top.map(|top| top.at).or(self.root());
        Shown { tip, covers, joined: self.joined, durable: self.durable.clone() }
    }

    /// First `lookahead` best heights with no node from the next send, and from the root while
    /// the stream sits below it: each held, folding or wanted (a new want = one `Fetch`)
    fn want(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let next = self.next_send();
        let mut asked = Vec::new();
        for from in [next, self.next_root().max(next)] {
            let heights = from.up_to(chain.best().height).map(|height| BlockRef {
                hash: chain.hash_at(height).expect("at or below the best tip"),
                height,
            });
            let unfolded = heights.filter(|at| !self.graph.contains(&at.hash));
            asked.extend(unfolded.take(self.lookahead));
        }
        asked.sort_by_key(|at| at.height);
        asked.dedup();
        let final_height = chain.final_tip().map(|tip| tip.height);
        for at in asked {
            let held = self.ready.contains_key(&at.height) || self.folding.contains_key(&at.hash);
            if held || self.wanted.contains_key(&at.height) {
                continue;
            }
            self.wanted.insert(at.height, at.hash);
            let record = chain.header_at(at.height).expect("a wanted height is best");
            let urgency = match Some(at.height) > final_height {
                true => Urgency::Tip,
                false => Urgency::Bulk,
            };
            out.push(Output::Fetch { at, record, urgency });
        }
    }

    /// N1–N5, J1–J4 and the fetch bookkeeping; panics naming the invariant broken
    pub(crate) fn check(&self) {
        let Some(chain) = &self.chain else {
            let empty = self.graph.is_empty() && self.ready.is_empty() && self.folding.is_empty();
            let empty = empty && self.wanted.is_empty();
            assert!(empty && self.shown.tip.is_none(), "nothing before a chain");
            return;
        };
        let whole = self.groups.iter().all(|group| {
            let joined = self.joined.covers(*group);
            joined || group.iter().all(|index| !self.joined.contains(index))
        });
        assert!(whole && !self.joined.is_empty(), "J1: joined = whole groups, never none");
        let root = self.root();
        for group in self.groups.iter().filter(|group| !self.joined.covers(**group)) {
            let at_root = group.iter().all(|index| self.durable[index] == root);
            assert!(!at_root, "J2: a lagging group at the root joins");
        }
        assert!(self.undelivered <= self.lookahead, "N5: at most lookahead sends in flight");
        let sent = self.sent.map(|sent| sent.at);
        let lowest = self.lowest();
        assert!(height(lowest) <= height(sent), "N3: the stream at or past the lowest durable tip");
        let final_height = height(chain.final_tip());
        if height(sent) > final_height {
            assert_eq!(sent, lowest, "N5: nothing sent past the final tip");
        } else if let Some(sent) = sent {
            assert!(on_best(chain, sent), "N5: a sent block is never retracted");
        }

        self.graph.check(chain, root, self.joined);
        if let Some(Sent { at, folded: true }) = self.sent {
            for height in self.next_root().up_to(at.height) {
                let hash = chain.hash_at(height).expect("sent = final");
                let held = self.graph.contains(&hash);
                assert!(held, "N3: a node sent final stays until every joined index holds it");
            }
        }
        if self.restarting(chain) {
            assert_eq!(self.shown.tip, None, "N4: nothing served while restarting");
        } else {
            let serving = self.serving(chain);
            assert_eq!(
                self.shown.tip, serving.tip,
                "N4: served tip = the deepest folded best block"
            );
            let (shown, now) =
                ((self.shown.covers, self.shown.joined), (serving.covers, self.joined));
            assert_eq!(shown, now, "N4: published indexes = the served block's, the joined");
        }

        let next = self.next_send();
        for (height, body) in &self.ready {
            assert!(*height >= next, "fetch: ready bodies above the last sent");
            assert!(on_best(chain, body.at()), "N1: every ready body is best");
            assert!(!self.graph.contains(&body.at().hash), "fetch: a folded block is not ready");
            assert!(!self.wanted.contains_key(height), "fetch: a ready body is not wanted");
        }
        for (hash, (_, covers)) in &self.folding {
            let held = self.graph.get(hash).is_some_and(|node| node.covers.covers(*covers));
            assert!(!held, "fold: nothing folds twice");
            assert!(self.joined.covers(*covers), "J3: a fold covers joined indexes only");
        }
        for (height, hash) in &self.wanted {
            assert!(*height >= next, "fetch: wants above the last sent");
            assert_eq!(chain.hash_at(*height), Some(*hash), "N1: every want is best");
            assert!(!self.graph.contains(hash), "fetch: a folded block is not wanted");
        }
    }
}

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model;
