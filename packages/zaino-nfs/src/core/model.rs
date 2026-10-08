//! [`NfsCore`] against naive final-path writers and an oracle fold (`nfs.md`), `check()` after
//! every step
//!
//! - Chain moves (a real [`HeaderChain`], work varying per branch): extend, reorg at a random depth
//!   above final onto a heavier branch (longer, same height, or a retreat), an earlier best made
//!   heaviest again (nodes reused), finalize
//! - Each move published or coalesced with the next (a `watch` keeps the latest)
//! - Fetches answered late (random delay), out of order, or after their abandon (a stale body)
//! - Folds answered after a random delay, in random order
//! - Each writer commits the final prefix after its own random delay (the final path)
//! - Restarts: fresh core from the writers' durable tips (`reset` = header store lost too,
//!   `wipe` = one index's directory deleted: it holds the served tip back until it catches up)
//! - Oracle ([`Toy`]) = fold from genesis along each block's own path

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::{HeaderChain, Record, Rejected, VerifiedChain};
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_sync::{check_block, Checked};

use super::{Indexes, Input, NfsCore, Output, SnapshotTip};
use crate::graph::on_best;
use crate::snapshot::Branch;

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
/// Fold window (the driver's `2 · depth`)
const WINDOW: u32 = 6;
const INDEXES: usize = 3;
/// Most virtual seconds the balancer takes to serve a body (hedges, retries, a bench)
const SERVED_WITHIN: u64 = 20;
/// Virtual seconds a case may take to settle once the moves end
const SETTLE: u32 = 20_000;

/// - `Reorg` = top `depth` replaced by `len` blocks, the first outweighing them (`len` < `depth` =
///   a retreat)
/// - `Revive` = an earlier best tip (`pick` mod held) outweighs the best again (switch back)
#[derive(Debug, Clone)]
enum Change {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Revive { pick: u8 },
    Finalize,
}

/// - `Chain.publish` = false: coalesced with the next publish
/// - `Advance` = seconds, each a tick + every answer, fold and commit due
#[derive(Debug, Clone)]
enum Move {
    Chain { change: Change, publish: bool },
    Advance(u8),
    Restart { reset: bool, wipe: Option<u8> },
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let change = prop_oneof![
        3 => (1u32..=6).prop_map(Change::Extend),
        3 => (1u32..=6, 1u32..=6).prop_map(|(depth, len)| Change::Reorg { depth, len }),
        2 => any::<u8>().prop_map(|pick| Change::Revive { pick }),
        2 => Just(Change::Finalize),
    ];
    let one = prop_oneof![
        8 => (change, prop::bool::weighted(0.7))
            .prop_map(|(change, publish)| Move::Chain { change, publish }),
        6 => (0u8..=30).prop_map(Move::Advance),
        3 => (any::<bool>(), prop::option::weighted(0.7, any::<u8>()))
            .prop_map(|(reset, wipe)| Move::Restart { reset, wipe }),
    ];
    prop::collection::vec(one, 1..32)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// - `check()` after every step
    /// - One `Fetch` per want until its body or its `Abandon`
    /// - Folds: exactly the indexes durable below the block, each on its oracle parent (a node's,
    ///   or durable exactly below it); none while the root trails best past the window
    /// - Published tip on the best; every index = the oracle (node) or durable at or past it
    /// - Settled: every index durable through the final tip, served at best
    #[test]
    fn the_served_tip_follows_the_verified_best_through_reorgs_restarts_and_bulk_sync(
        moves in moves(),
        delays in prop::collection::vec(0u64..=20, 1..=INDEXES),
        lookahead in 1usize..=4,
        seed in any::<u64>(),
    ) {
        run(&moves, &delays, lookahead, seed);
    }
}

/// Toy fold payload: height + running digest of every hash on the path
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Toy {
    height: Height,
    digest: u64,
}

/// One node's payload: `Some` per index it folds
type Toys = Vec<Option<Toy>>;

/// FNV-1a over the block hash, seeded by the parent's digest
fn fold(parent: Option<Toy>, block: &Block) -> Toy {
    let header = block.header();
    if let Some(parent) = parent {
        assert_eq!(parent.height.next(), header.height, "toy fold on its parent");
    }
    let seed = parent.map_or(0xcbf2_9ce4_8422_2325, |parent| parent.digest);
    let bytes = <[u8; 32]>::from(header.hash);
    let digest = bytes
        .iter()
        .fold(seed, |digest, byte| (digest ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3));
    Toy { height: header.height, digest }
}

/// Final-path writer: `durable` = committed tip (a final block), `committing` = a commit due
#[derive(Debug)]
struct Writer {
    durable: Option<BlockRef>,
    delay: u64,
    committing: bool,
}

#[derive(Debug)]
enum Due {
    Body(Checked),
    Folded { at: BlockRef, covers: Indexes, toys: Toys },
    Commit(usize),
}

/// - `fetching` = blocks with a `Fetch` out, neither answered nor abandoned
/// - `decoy` = a block off every chain (G7: `at` of it = `None`)
/// - `published` = last published tip; `told` = the durable tips last given to the core
struct Sim {
    builder: MockChain,
    headers: HeaderChain,
    decoy: BlockHash,
    lookahead: usize,
    core: NfsCore<Toys>,
    given: Option<Arc<VerifiedChain>>,
    pending: Vec<(Instant, Due)>,
    fetching: HashSet<BlockHash>,
    writers: Vec<Writer>,
    told: Vec<Option<BlockRef>>,
    published: Option<BlockRef>,
    tips: Vec<BlockHash>,
    oracle: HashMap<BlockHash, Toy>,
    now: Instant,
    rng: u64,
}

impl Sim {
    fn random(&mut self, below: u64) -> u64 {
        // xorshift64*: deterministic from the case's seed
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) % below.max(1)
    }

    fn later(&mut self, most: u64) -> Instant {
        self.now + Duration::from_secs(self.random(most + 1))
    }

    fn oracle(&mut self, hash: BlockHash) -> Toy {
        if let Some(toy) = self.oracle.get(&hash) {
            return *toy;
        }
        let block = Arc::clone(self.builder.block(hash));
        let header = block.header();
        let parent = (header.height != Height::GENESIS).then(|| self.oracle(header.prev_hash));
        let toy = fold(parent, &block);
        self.oracle.insert(hash, toy);
        toy
    }

    /// Genesis ..= the header chain's best, from the builder (the chain keeps no deep history)
    fn best_path(&self) -> Vec<BlockHash> {
        let best = self.headers.best().expect("anchored at genesis").block;
        self.builder.blocks(best).iter().map(|block| block.header().hash).collect()
    }

    fn durable(&self) -> Vec<Option<BlockRef>> {
        self.writers.iter().map(|writer| writer.durable).collect()
    }

    /// The core's rule, naively: the lowest durable tip within `WINDOW` of best
    fn active(&self) -> bool {
        let Some(chain) = &self.given else { return false };
        let lowest = self.told.iter().map(|tip| tip.map_or(0, |tip| u32::from(tip.height) + 1));
        let next = lowest.min().unwrap_or(0);
        (u32::from(chain.best().height) + 1).saturating_sub(next) <= WINDOW
    }

    fn durable_floor(&self) -> u32 {
        let tips = self.writers.iter().filter_map(|writer| writer.durable);
        tips.map(|tip| u32::from(tip.height)).max().unwrap_or(0)
    }

    fn change(&mut self, change: &Change) {
        let best = self.headers.best().expect("genesis verified").block;
        // reset header store forgets finality (durable tips still bound a fork)
        let final_height = self.headers.final_tip().map_or(0, |tip| u32::from(tip.height));
        let floor = final_height.max(self.durable_floor());
        let (fork, tip) = match *change {
            Change::Extend(count) => {
                (best.height, self.builder.branch(best).mine_empty(count).tip())
            }
            Change::Reorg { depth, len } => {
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let parent = self.builder.block(self.best_path()[at as usize]).at();
                (height(at), self.builder.branch(parent).outweigh().mine_empty(len).tip())
            }
            Change::Revive { pick } => {
                let Some(&old) = self.tips.get(usize::from(pick) % self.tips.len().max(1)) else {
                    return;
                };
                let path = self.best_path();
                let branch = self.builder.blocks(self.builder.block(old).at());
                let shared = branch.iter().zip(&path).take_while(|(b, p)| b.header().hash == **p);
                let fork = shared.count() - 1;
                if (fork as u32) < floor || fork + 1 == branch.len() {
                    return;
                }
                let old = self.builder.block(old).at();
                let heavy = self.builder.branch(old).outweigh().mine_empty(1).tip();
                let mined = &self.builder.blocks(heavy)[fork + 1..];
                // a long side branch re-entering past the header chain's side bound: evicted (H4)
                if let Err(Rejected::Orphan) = insert(&mut self.headers, mined) {
                    return;
                }
                (height(fork as u32), heavy)
            }
            Change::Finalize => {
                if let Some(boundary) = self.headers.finalizable() {
                    self.headers.finalize(boundary);
                }
                return;
            }
        };
        let mined = &self.builder.blocks(tip)[u32::from(fork) as usize + 1..];
        insert(&mut self.headers, mined).expect("valid headers");
        let now = self.headers.best().expect("verified").block;
        assert_eq!(now, tip, "sim: the heavier branch is best");
        if fork < best.height {
            self.tips.push(best.hash);
        }
    }

    fn publish(&mut self, context: &str) {
        let chain = Arc::new(self.headers.verified().expect("genesis verified"));
        self.given = Some(Arc::clone(&chain));
        self.step(Input::Chain(chain), context);
    }

    /// A fresh core, as the driver boots one: the chain, then every durable tip
    fn boot(&mut self, context: &str) {
        self.core = NfsCore::new(self.lookahead, self.writers.len(), WINDOW);
        self.told = vec![None; self.writers.len()];
        self.publish(context);
        self.step(Input::Durable(self.durable()), context);
    }

    fn step(&mut self, input: Input<Toys>, context: &str) {
        if let Input::Durable(tips) = &input {
            self.told = tips.clone();
        }
        let outputs = self.core.step(input);
        self.core.check();
        for output in outputs {
            match output {
                Output::Fetch { at, record } => self.ask(at, record, context),
                Output::Abandon(at) => self.abandon(at, context),
                Output::Fold { at, parent, block, covers } => {
                    self.fold(at, parent, &block, covers, context)
                }
                Output::Publish(Some(tip)) => self.served(tip, context),
                Output::Publish(None) => self.published = None,
            }
        }
        let wanted: HashSet<BlockHash> = self.core.wanted.values().copied().collect();
        assert_eq!(wanted, self.fetching, "{context}: one Fetch out per want, none past it");
        self.verify(context);
    }

    /// The balancer's checked answer, served within [`SERVED_WITHIN`]
    fn ask(&mut self, at: BlockRef, record: Record, context: &str) {
        assert_eq!(record.hash, at.hash, "{context}: Fetch {at:?} with another's header");
        let fresh = self.fetching.insert(at.hash);
        assert!(fresh, "{context}: Fetch {at:?} twice while in flight");
        let honest = Block::clone(self.builder.block(at.hash));
        let checked = check_block(honest, at.height, &record).expect("the asked block passes");
        let due = self.later(SERVED_WITHIN);
        self.pending.push((due, Due::Body(checked)));
    }

    /// Its fetch dropped; the answer still lands half the time (a stale body: ignored)
    fn abandon(&mut self, at: BlockRef, context: &str) {
        assert!(self.fetching.remove(&at.hash), "{context}: Abandon {at:?} with no Fetch out");
        if self.random(2) == 0 {
            let answer = |due: &Due| matches!(due, Due::Body(body) if body.at() == at);
            self.pending.retain(|(_, due)| !answer(due));
        }
    }

    /// - Inside the window only
    /// - Covers exactly the indexes durable below `at`
    /// - Each on the oracle's parent (the parent node's, or durable exactly below `at`)
    fn fold(
        &mut self,
        at: BlockRef,
        parent: Option<Arc<Toys>>,
        block: &Block,
        covers: Indexes,
        context: &str,
    ) {
        assert!(self.active(), "{context}: N5 Fold {at:?} outside the window");
        let expected =
            (at.height != Height::GENESIS).then(|| self.oracle(block.header().prev_hash));
        let mut toys: Toys = vec![None; self.writers.len()];
        for index in 0..self.writers.len() {
            let durable = self.told[index];
            let below = durable.map(|tip| tip.height) < Some(at.height);
            assert_eq!(covers.contains(index), below, "{context}: N2 Fold {at:?} index {index}");
            if !below {
                continue;
            }
            let parent = match parent.as_ref().and_then(|parent| parent[index]) {
                Some(parent) => Some(parent),
                None => {
                    let at_parent = durable.map(|tip| tip.height) == at.height.checked_sub(1);
                    assert!(at_parent, "{context}: N2 Fold {at:?} index {index} on {durable:?}");
                    expected
                }
            };
            assert_eq!(parent, expected, "{context}: N6 Fold {at:?} index {index} parent");
            toys[index] = Some(fold(parent, block));
        }
        let due = self.later(4);
        self.pending.push((due, Due::Folded { at, covers, toys }));
    }

    /// - N4: on its chain's best; every index = the oracle (node) or durable at or past it
    /// - G7: `at` of every node, the root and the decoy = the naive answer
    fn served(&mut self, tip: SnapshotTip<Toys>, context: &str) {
        let at = tip.tip;
        assert!(on_best(&tip.chain, at), "{context}: N4 served {at:?} off its best");
        let toys = tip.graph.at(&tip.chain, tip.root, &at.hash).and_then(|base| base.folded);
        let expected = Some(self.oracle(at.hash));
        for index in 0..self.writers.len() {
            let durable = self.told[index].map(|tip| tip.height) >= Some(at.height);
            let folded = toys.as_ref().and_then(|toys| toys[index]);
            assert!(
                durable || folded == expected,
                "{context}: N4 served {at:?} index {index}: folded {folded:?}"
            );
        }
        let nodes = tip.graph.nodes().map(|node| node.at.hash);
        let asked: Vec<BlockHash> =
            nodes.chain(tip.root.map(|root| root.hash)).chain([self.decoy]).collect();
        for hash in asked {
            let got = tip.graph.at(&tip.chain, tip.root, &hash);
            let got = got.map(|base| (base.at, base.branch, base.folded.is_some()));
            let expected = self.at(&tip, hash);
            assert_eq!(got, expected, "{context}: G7 at {hash:?}");
        }
        self.published = Some(at);
    }

    /// Naive `at`: the root unfolded; a node folded, branch = its path vs the chain's
    fn at(&self, tip: &SnapshotTip<Toys>, hash: BlockHash) -> Option<(BlockRef, Branch, bool)> {
        if let Some(root) = tip.root.filter(|root| root.hash == hash) {
            return Some((root, Branch::Best, false));
        }
        tip.graph.get(&hash)?;
        let blocks = self.builder.blocks(self.builder.block(hash).at());
        let path: Vec<BlockRef> = blocks.iter().map(|block| block.at()).collect();
        let shared = path.iter().take_while(|at| on_best(&tip.chain, **at)).count();
        let branch = match path.get(shared) {
            None => Branch::Best,
            Some(_) => Branch::Side { from: path[shared - 1] },
        };
        Some((path[path.len() - 1], branch, true))
    }

    /// After every step: every node's folds = the oracle; no node outside the window
    fn verify(&mut self, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any step"));
        let served = self.published;
        assert!(served.is_none_or(|tip| on_best(&chain, tip)), "{context}: N4 served off best");
        let nodes: Vec<(BlockRef, Toys)> =
            self.core.graph.nodes().map(|node| (node.at, (*node.folded).clone())).collect();
        assert!(nodes.is_empty() || self.active(), "{context}: N5 nodes outside the window");
        for (at, toys) in nodes {
            let toy = Some(self.oracle(at.hash));
            let wrong = toys.iter().flatten().any(|folded| Some(*folded) != toy);
            assert!(!wrong, "{context}: N6 node {at:?} folded {toys:?}, oracle {toy:?}");
        }
    }

    /// Each writer behind the final tip, no commit due: one scheduled after its delay
    fn schedule_commits(&mut self) {
        let final_tip = self.given.as_ref().map(|chain| chain.final_tip());
        for index in 0..self.writers.len() {
            let writer = &self.writers[index];
            let behind = final_tip.map(|tip| tip.height) > writer.durable.map(|tip| tip.height);
            if behind && !writer.committing {
                let due = self.later(writer.delay);
                self.writers[index].committing = true;
                self.pending.push((due, Due::Commit(index)));
            }
        }
    }

    /// Every body, fold and commit due by now, in a random order
    fn answer(&mut self, context: &str) {
        loop {
            self.schedule_commits();
            let due: Vec<usize> =
                (0..self.pending.len()).filter(|at| self.pending[*at].0 <= self.now).collect();
            if due.is_empty() {
                return;
            }
            let pick = due[self.random(due.len() as u64) as usize];
            let input = match self.pending.swap_remove(pick).1 {
                Due::Body(body) => {
                    let at = body.at();
                    if self.core.wanted.get(&at.height) == Some(&at.hash) {
                        self.fetching.remove(&at.hash);
                    }
                    Input::Body(body)
                }
                Due::Folded { at, covers, toys } => {
                    Input::Folded { at, covers, folded: Arc::new(toys) }
                }
                Due::Commit(index) => {
                    let final_tip = self.given.as_ref().map(|chain| chain.final_tip());
                    let writer = &mut self.writers[index];
                    writer.committing = false;
                    if final_tip.map(|tip| tip.height) > writer.durable.map(|tip| tip.height) {
                        writer.durable = final_tip;
                    }
                    Input::Durable(self.durable())
                }
            };
            self.step(input, context);
        }
    }

    /// One virtual second, then everything due
    fn advance(&mut self, context: &str) {
        self.now += Duration::from_secs(1);
        self.answer(context);
    }

    /// - Everything not durable lost: in-flight fetches, folds, commits, the core
    /// - `wipe` (mod the index count) = that index's store too
    fn restart(&mut self, reset: bool, wipe: Option<u8>, context: &str) {
        if reset {
            let tip = self.builder.block(*self.best_path().last().expect("genesis")).at();
            self.headers = self.builder.header_chain(DEPTH);
            let path = self.builder.blocks(tip);
            insert(&mut self.headers, &path).expect("the best path verifies");
        }
        if let Some(wipe) = wipe {
            let count = self.writers.len();
            self.writers[usize::from(wipe) % count].durable = None;
        }
        for writer in &mut self.writers {
            writer.committing = false;
        }
        self.pending.clear();
        self.fetching.clear();
        self.published = None;
        self.boot(context);
    }

    /// Liveness: every index durable through the final tip, served at best
    fn settle(&mut self) {
        // final past every durable tip (a reset header store may sit below them)
        self.change(&Change::Extend(DEPTH.get()));
        self.change(&Change::Finalize);
        self.publish("settle");
        let chain = Arc::clone(self.given.as_ref().expect("published"));
        let best = chain.best();
        for second in 0..SETTLE {
            let durable =
                self.writers.iter().all(|writer| writer.durable == Some(chain.final_tip()));
            if durable && self.published == Some(best) {
                return;
            }
            self.advance(&format!("settle {second}s"));
        }
        let durable: Vec<Option<BlockRef>> =
            self.writers.iter().map(|writer| writer.durable).collect();
        panic!(
            "liveness: never settled: durable {durable:?}, served {:?} of {best:?}",
            self.published
        );
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small chain")
}

fn run(moves: &[Move], delays: &[u64], lookahead: usize, seed: u64) {
    let mut builder = MockChain::regtest().varied_work();
    let genesis = builder.genesis();
    let decoy = builder.branch(genesis).mine_empty(1).tip().hash;
    let headers = builder.header_chain(DEPTH);
    let writers: Vec<Writer> =
        delays.iter().map(|&delay| Writer { durable: None, delay, committing: false }).collect();
    let mut sim = Sim {
        builder,
        headers,
        decoy,
        lookahead,
        core: NfsCore::new(lookahead, writers.len(), WINDOW),
        given: None,
        pending: Vec::new(),
        fetching: HashSet::new(),
        writers,
        told: Vec::new(),
        published: None,
        tips: Vec::new(),
        oracle: HashMap::new(),
        now: Instant::now(),
        rng: seed | 1,
    };
    sim.boot("start");
    for (at, step) in moves.iter().enumerate() {
        let context = format!("move {at} {step:?}");
        match step {
            Move::Chain { change, publish } => {
                sim.change(change);
                if *publish {
                    sim.publish(&context);
                }
                sim.answer(&context);
            }
            Move::Advance(seconds) => (0..*seconds).for_each(|_| sim.advance(&context)),
            Move::Restart { reset, wipe } => sim.restart(*reset, *wipe, &context),
        }
    }
    sim.settle();
}
