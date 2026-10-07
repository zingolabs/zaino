//! [`NfsCore`]: the [`VerifiedChain`] + bodies + folds + durable tips → the final stream, the
//! served tip and the fetches and folds feeding them (`nfs.md` §6, §9)
//!
//! - Pure: no I/O, no clock (`now` = an input)
//! - Fetch, check, fold, send = the driver's
//! - Root = lowest durable tip of every index, nodes only above it
//! - Final + no folded parent → sent unfolded (writers fold)
//! - Else folded parent first as it joins the best → sent folded once final (lockstep finality)
//! - Served tip = deepest folded best block, else the root (reorg → fork point at once)

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};

use crate::fetch::{Answer, Checked, Fetcher, Misanswer};
use crate::graph::{on_best, Graph, Node};

/// Folded payload `F` = one block's folded state per index (`Folded`, toy in the model)
pub(crate) struct NfsCore<F> {
    lookahead: usize,
    chain: Option<Arc<VerifiedChain>>,
    durable: Vec<Option<BlockRef>>,
    sent: Option<Sent>,
    graph: Graph<F>,
    ready: BTreeMap<Height, Checked>,
    folding: HashMap<BlockHash, Checked>,
    fetcher: Fetcher,
    served: Option<BlockRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sent {
    at: BlockRef,
    folded: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body { from: usize, at: BlockRef, answer: Answer },
    Folded { at: BlockRef, folded: Arc<F> },
    Durable { index: usize, tip: Option<BlockRef> },
    Tick,
}

/// - `Send`s in list order; the rest in any
/// - `Fold.parent` = `None`: fold on the committed stores at the root
#[derive(Debug, Clone)]
pub(crate) enum Output<F> {
    Fetch { from: usize, height: Height, record: Record },
    Misanswered { from: usize, at: BlockRef, why: Misanswer },
    Unserved { height: Height },
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block> },
    Send(Final<F>),
    Publish(SnapshotTip<F>),
}

/// One final-stream step (`folded` = `None`: the writer folds it)
#[derive(Debug, Clone)]
pub(crate) struct Final<F> {
    pub(crate) block: Arc<Block>,
    pub(crate) folded: Option<Arc<F>>,
}

/// `folded` = `None`: the root, read from the committed stores alone
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTip<F> {
    pub(crate) chain: Arc<VerifiedChain>,
    pub(crate) tip: BlockRef,
    pub(crate) folded: Option<Arc<F>>,
}

/// Index `index`'s durable block off the final chain (resync required)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Diverged {
    pub(crate) index: usize,
    pub(crate) height: Height,
    pub(crate) expected: BlockHash,
    pub(crate) got: BlockHash,
}

impl<F> NfsCore<F> {
    /// - `durable` = each enabled index's durable tip (`Durable.index` = its position)
    /// - `lookahead` = bodies fetched or folding ahead of the next one needed
    pub(crate) fn new(
        sources: usize,
        lookahead: usize,
        depth: ReorgDepth,
        durable: Vec<Option<BlockRef>>,
    ) -> Self {
        assert!(lookahead > 0, "at least one block in flight");
        assert!(!durable.is_empty(), "an index to feed");
        let mut core = Self {
            lookahead,
            chain: None,
            durable,
            sent: None,
            graph: Graph::new(depth.get()),
            ready: BTreeMap::new(),
            folding: HashMap::new(),
            fetcher: Fetcher::new(sources),
            served: None,
        };
        core.sent = core.root().map(|at| Sent { at, folded: false });
        core
    }

    /// `Err` = an index's durable block off the final chain (resync required)
    pub(crate) fn step(
        &mut self,
        input: Input<F>,
        now: Instant,
    ) -> Result<Vec<Output<F>>, Diverged> {
        let mut out = Vec::new();
        match input {
            Input::Chain(chain) => self.follow(chain)?,
            Input::Body { from, at, answer } => {
                if let Some(body) = self.fetcher.answered(from, at, answer, now, &mut out) {
                    self.ready.insert(at.height, body);
                }
            }
            Input::Folded { at, folded } => self.folded(at, folded),
            Input::Durable { index, tip } => self.durable(index, tip),
            Input::Tick => {}
        }
        let Some(chain) = self.chain.clone() else { return Ok(out) };
        self.graph.prune(&chain, self.root());
        self.forget(&chain);
        if !self.restarting(&chain) {
            self.send(&chain, &mut out);
            self.fold(&chain, &mut out);
            self.publish(&chain, &mut out);
        }
        self.want(&chain);
        self.fetcher.ask(&chain, now, &mut out);
        Ok(out)
    }

    /// Lowest durable tip (`None` = an index holds nothing)
    fn root(&self) -> Option<BlockRef> {
        let lowest = self.durable.iter().min_by_key(|tip| tip.map(|tip| tip.height));
        lowest.copied().flatten()
    }

    fn next_send(&self) -> Height {
        self.sent.map_or(Height::GENESIS, |sent| sent.at.height.next())
    }

    /// Durable tip above the final tip (lost header store): nothing sent, folded or served
    /// meanwhile
    fn restarting(&self, chain: &VerifiedChain) -> bool {
        let final_height = chain.final_tip().map(|tip| tip.height);
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
        let final_height = chain.final_tip().map(|tip| tip.height);
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

    fn folded(&mut self, at: BlockRef, folded: Arc<F>) {
        let body = self.folding.remove(&at.hash).expect("a fold result for a fold in flight");
        let parent = body.block().header().prev_hash;
        let root = self.root();
        if Some(at.height) > root.map(|root| root.height)
            && self.graph.holds_parent(root, parent, at.height)
        {
            self.graph.insert(Node { at, parent, block: Arc::clone(body.block()), folded });
        }
    }

    fn durable(&mut self, index: usize, tip: Option<BlockRef>) {
        assert!(index < self.durable.len(), "a durable tip of an enabled index");
        let height = |tip: Option<BlockRef>| tip.map(|tip| tip.height);
        assert!(height(tip) >= height(self.durable[index]), "N3: a durable tip never moves back");
        if tip != self.durable[index] {
            let sent = self.sent.map(|sent| sent.at.height);
            assert!(height(tip) <= sent, "N3: a durable tip is a block the stream sent");
        }
        self.durable[index] = tip;
    }

    /// Bodies and wants no longer needed: off best, already sent, or folded
    fn forget(&mut self, chain: &VerifiedChain) {
        let next = self.next_send();
        let graph = &self.graph;
        let needed =
            |at: BlockRef| at.height >= next && on_best(chain, at) && !graph.contains(&at.hash);
        self.ready.retain(|_, body| needed(body.at()));
        self.fetcher.retain(needed);
    }

    /// Final heights in order: a node's folded, else (no folded parent, no fold in flight) the
    /// body unfolded
    fn send(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        loop {
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
                    let below = height.checked_sub(1).and_then(|below| chain.hash_at(below));
                    if below.is_some_and(|below| self.graph.contains(&below))
                        || self.folding.contains_key(&hash)
                    {
                        return;
                    }
                    let Some(body) = self.ready.remove(&height) else { return };
                    out.push(Output::Send(Final { block: Arc::clone(body.block()), folded: None }));
                    false
                }
            };
            self.sent = Some(Sent { at: BlockRef { hash, height }, folded });
        }
    }

    /// Next best block above the run, once its parent is folded
    ///
    /// - On the root: only above the final tip (root ≤ sent ≤ final ⇒ root = sent: lockstep)
    fn fold(&mut self, chain: &VerifiedChain, out: &mut Vec<Output<F>>) {
        let top = self.graph.best_top(chain, self.root());
        let height = top.map_or(self.next_root(), |top| top.at.height.next());
        let Some(hash) = chain.hash_at(height) else { return };
        if self.folding.contains_key(&hash) {
            return;
        }
        let parent = match top {
            Some(top) => Some(Arc::clone(&top.folded)),
            None if Some(height) <= chain.final_tip().map(|tip| tip.height) => return,
            None => None,
        };
        let Some(body) = self.ready.remove(&height) else { return };
        let block = Arc::clone(body.block());
        out.push(Output::Fold { at: BlockRef { hash, height }, parent, block });
        self.folding.insert(hash, body);
    }

    fn next_root(&self) -> Height {
        self.root().map_or(Height::GENESIS, |root| root.height.next())
    }

    /// Served tip moved → published (deepest folded best block, else the root)
    fn publish(&mut self, chain: &Arc<VerifiedChain>, out: &mut Vec<Output<F>>) {
        let (tip, folded) = self.serving(chain);
        if tip == self.served {
            return;
        }
        self.served = tip;
        if let Some(tip) = tip {
            out.push(Output::Publish(SnapshotTip { chain: Arc::clone(chain), tip, folded }));
        }
    }

    fn serving(&self, chain: &VerifiedChain) -> (Option<BlockRef>, Option<Arc<F>>) {
        match self.graph.best_top(chain, self.root()) {
            Some(top) => (Some(top.at), Some(Arc::clone(&top.folded))),
            None => (self.root(), None),
        }
    }

    /// First `lookahead` best heights from the next send with no node: each held, folding or
    /// wanted
    fn want(&mut self, chain: &VerifiedChain) {
        let heights = self.next_send().up_to(chain.best().height);
        let at = heights.map(|height| BlockRef {
            hash: chain.hash_at(height).expect("at or below the best tip"),
            height,
        });
        let unfolded = at.filter(|at| !self.graph.contains(&at.hash));
        for at in unfolded.take(self.lookahead).collect::<Vec<_>>() {
            if !self.ready.contains_key(&at.height) && !self.folding.contains_key(&at.hash) {
                self.fetcher.want(at);
            }
        }
    }

    /// N1–N5 and the fetch bookkeeping; panics naming the invariant broken
    pub(crate) fn check(&self) {
        let Some(chain) = &self.chain else {
            let empty = self.graph.is_empty() && self.ready.is_empty() && self.folding.is_empty();
            assert!(empty && self.served.is_none(), "nothing before a chain");
            return;
        };
        let root = self.root();
        let height = |tip: Option<BlockRef>| tip.map(|tip| tip.height);
        let sent = self.sent.map(|sent| sent.at);
        assert!(height(root) <= height(sent), "N3: root at or below the last block sent");
        let final_height = height(chain.final_tip());
        if height(sent) > final_height {
            assert_eq!(sent, root, "N5: nothing sent past the final tip");
        } else if let Some(sent) = sent {
            assert!(on_best(chain, sent), "N5: a sent block is never retracted");
        }

        self.graph.check(chain, root);
        if let Some(Sent { at, folded: true }) = self.sent {
            for height in self.next_root().up_to(at.height) {
                let hash = chain.hash_at(height).expect("sent = final");
                let held = self.graph.contains(&hash);
                assert!(held, "N3: a node sent final stays until every index holds it durably");
            }
        }
        let serving = if self.restarting(chain) { None } else { self.serving(chain).0 };
        assert_eq!(self.served, serving, "N4: served tip = the deepest folded best block");

        let next = self.next_send();
        for (height, body) in &self.ready {
            assert!(*height >= next, "fetch: ready bodies above the last sent");
            assert!(on_best(chain, body.at()), "N1: every ready body is best");
            assert!(!self.graph.contains(&body.at().hash), "fetch: a folded block is not ready");
            assert!(!self.fetcher.wants(*height), "fetch: a ready body is not wanted");
        }
        for hash in self.folding.keys() {
            assert!(!self.graph.contains(hash), "fold: nothing folds twice");
        }
        self.fetcher.check(chain, next);
    }
}

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model;
