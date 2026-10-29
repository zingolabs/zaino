//! [`NfsCore`] against naive finalized-path writers and an oracle fold (`usage.md`), `check()`
//! after every step
//!
//! - Chain moves (a real [`HeaderChain`], work varying per branch): extend, reorg at a random depth
//!   above finalized onto a heavier branch (longer, same height, or a retreat), an earlier best
//!   made heaviest again (nodes reused), finalize
//! - Each move published or coalesced with the next (a `watch` keeps the latest)
//! - Fetches answered late (random delay), out of order, or after their abandon (a stale body)
//! - Folds answered after a random delay, in random order
//! - Each writer commits the finalized prefix after its own random delay (the finalized path)
//! - Restarts: fresh core from the writers' durable tips
//! - `reset` = header store lost too, then re-anchored `DEPTH` below best like HeaderSync,
//!   possibly above every durable tip
//! - `wipe` = one index's directory deleted (holds the served tip back until it catches up)
//! - Oracle ([`Toy`]) = fold from genesis along each block's own path

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use zaino_header_chain::testing::insert;
use zaino_header_chain::{decode_header, HeaderChain, HeaderChainSnapshot, Rejected};
use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{encode_header, h, MockChain, QuietPanics};
use zaino_sync::IndexKind;
use zaino_sync::{testing::check_block, Checked};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use super::{DurableTips, Input, NfsCore, Output, Publication};
use crate::per_index::{IndexSet, PerIndex};

const DEPTH: FinalityDepth = FinalityDepth::new(NonZeroU32::new(3).expect("nonzero"));
/// Fold window (the driver's `2 · depth`)
const WINDOW: u32 = 6;
const INDEXES: usize = 3;
/// Most virtual seconds the balancer takes to serve a body (hedges, retries, a bench)
const SERVED_WITHIN: u64 = 20;
/// Virtual seconds a case may take to settle once the moves end
const SETTLE: u32 = 20_000;

/// - `Reorg` = top `depth` replaced by `len` blocks, the first outweighing them (`len` < `depth` =
///   a retreat)
/// - `Revive` = the best tip replaced `back` reorgs ago (0 = the latest) outweighs the best again
///   (switch back; none that far back = no move)
#[derive(Debug, Clone)]
enum Change {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Revive { back: usize },
    Finalize,
}

/// - `Chain.publish` = false: coalesced with the next publish
/// - `Advance` = seconds, each a tick + every answer, fold and commit due
/// - `Restart.wipe` = that index's store deleted too
#[derive(Clone)]
enum Move {
    Chain { change: Change, publish: bool },
    Advance(u8),
    Restart { reset: bool, wipe: Option<usize> },
}

/// One line per move, even under `{:#?}` (shrunk case fits a screen)
impl std::fmt::Debug for Move {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Move::Chain { change, publish } => write!(f, "Chain({change:?}, publish: {publish})"),
            Move::Advance(seconds) => write!(f, "Advance({seconds})"),
            Move::Restart { reset, wipe } => write!(f, "Restart(reset: {reset}, wipe: {wipe:?})"),
        }
    }
}

/// `indexes` = writer count (`wipe` below it)
fn moves(indexes: usize) -> impl Strategy<Value = Vec<Move>> {
    let change = prop_oneof![
        3 => (1u32..=6).prop_map(Change::Extend),
        3 => (1u32..=6, 1u32..=6).prop_map(|(depth, len)| Change::Reorg { depth, len }),
        2 => (0usize..8).prop_map(|back| Change::Revive { back }),
        2 => Just(Change::Finalize),
    ];
    let one = prop_oneof![
        8 => (change, prop::bool::weighted(0.7))
            .prop_map(|(change, publish)| Move::Chain { change, publish }),
        6 => (0u8..=30).prop_map(Move::Advance),
        3 => (any::<bool>(), prop::option::weighted(0.7, 0..indexes))
            .prop_map(|(reset, wipe)| Move::Restart { reset, wipe }),
    ];
    prop::collection::vec(one, 1..32)
}

/// Per writer: its commit delay (virtual seconds), then the moves
fn histories() -> impl Strategy<Value = (Vec<u64>, Vec<Move>)> {
    let delays = prop::collection::vec(0u64..=20, 1..=INDEXES);
    delays.prop_flat_map(|delays| {
        let indexes = delays.len();
        (Just(delays), moves(indexes))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// - `check()` after every step
    /// - One `Fetch` per want until its body or its `Abandon`
    /// - Folds: exactly the indexes durable below the block, each on its oracle parent (a node's,
    ///   or durable exactly below it)
    /// - No folds while the root trails best past the window
    /// - Published tip on the best, every index = the oracle (node) or durable at or past it
    /// - Settled: every index durable through the finalized tip, served at best
    #[test]
    fn the_served_tip_follows_the_verified_best_through_reorgs_restarts_and_bulk_sync(
        (delays, moves) in histories(),
        lookahead in 1usize..=4,
        seed in any::<u64>(),
    ) {
        let _quiet = QuietPanics::hold();
        run(&moves, &delays, lookahead, seed);
    }
}

/// Toy fold payload: height + running digest of every hash on the path
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Toy {
    height: BlockHeight,
    digest: u64,
}

/// One node's payload: one per index it folds
type Toys = PerIndex<Toy>;

/// FNV-1a over the block hash, seeded by the parent's digest
fn fold(parent: Option<Toy>, block: &Block) -> Toy {
    let header = block.header();
    if let Some(parent) = parent {
        assert_eq!(parent.height + 1, header.height, "toy fold on its parent");
    }
    let seed = parent.map_or(0xcbf2_9ce4_8422_2325, |parent| parent.digest);
    let bytes = header.hash.0;
    let digest = bytes
        .iter()
        .fold(seed, |digest, byte| (digest ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3));
    Toy { height: header.height, digest }
}

/// Finalized-path writer: `durable` = durable tip (a finalized block), `committing` = a commit due
#[derive(Debug)]
struct Writer {
    durable: Option<BlockRef>,
    delay: u64,
    committing: bool,
}

#[derive(Debug)]
enum Due {
    Body(Checked),
    Folded { at: BlockRef, covers: IndexSet, toys: Toys },
    Commit(IndexKind),
}

/// - `fetching` = blocks with a `Fetch` out, neither answered nor abandoned
/// - `decoy` = a block off every chain (N7: `at` of it = `None`)
/// - `published` = last published tip
/// - `told` = the durable tips last given to the core
struct Sim {
    builder: MockChain,
    headers: HeaderChain,
    decoy: BlockHash,
    lookahead: usize,
    core: NfsCore<Toys>,
    given: Option<Arc<HeaderChainSnapshot>>,
    pending: Vec<(Instant, Due)>,
    fetching: HashSet<BlockHash>,
    writers: PerIndex<Writer>,
    told: DurableTips,
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
        let parent = (header.height != H0).then(|| self.oracle(header.prev_hash));
        let toy = fold(parent, &block);
        self.oracle.insert(hash, toy);
        toy
    }

    fn durable(&self) -> DurableTips {
        self.writers.iter().map(|(kind, writer)| (kind, writer.durable)).collect()
    }

    fn told_heights(&self) -> PerIndex<Option<BlockHeight>> {
        self.told.iter().map(|(kind, tip)| (kind, tip.map(|tip| tip.height))).collect()
    }

    /// The core's rule, naively: the lowest durable tip within `WINDOW` of best, the header above
    /// it held
    fn active(&self) -> bool {
        let Some(chain) = &self.given else { return false };
        let next = |tip: &Option<BlockRef>| tip.map_or(0, |tip| u32::from(tip.height) + 1);
        let lowest = self.told.iter().map(|(_, tip)| next(tip));
        let next = lowest.min().unwrap_or(0);
        let best = u32::from(chain.best().height);
        let headed = next > best || chain.header_at(h(next)).is_some();
        headed && (best + 1).saturating_sub(next) <= WINDOW
    }

    fn durable_floor(&self) -> u32 {
        let tips = self.writers.iter().filter_map(|(_, writer)| writer.durable);
        tips.map(|tip| u32::from(tip.height)).max().unwrap_or(0)
    }

    fn change(&mut self, change: &Change) {
        let best = self.headers.best().expect("genesis verified").block;
        // reset header store forgets finality (durable tips still bound a fork)
        let finalized_height = self.headers.finalized_tip().map_or(0, |tip| u32::from(tip.height));
        let floor = finalized_height.max(self.durable_floor());
        let (fork, tip) = match *change {
            Change::Extend(count) => {
                (best.height, self.builder.branch(best).mine_empty(count).tip())
            }
            Change::Reorg { depth, len } => {
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let parent = self.builder.ancestor(best, h(at));
                (parent.height, self.builder.branch(parent).outweigh().mine_empty(len).tip())
            }
            Change::Revive { back } => {
                let Some(&old) = self.tips.iter().rev().nth(back) else { return };
                let old = self.builder.block(old).block_ref();
                let fork = self.builder.fork_point(old, best);
                if u32::from(fork.height) < floor || fork == old {
                    return;
                }
                let heavy = self.builder.branch(old).outweigh().mine_empty(1).tip();
                let path = self.builder.blocks(heavy);
                let mined = &path[path.partition_point(|b| b.header().height <= fork.height)..];
                // a long side branch re-entering past the header chain's side bound: evicted (H4)
                if let Err(Rejected::Orphan) = insert(&mut self.headers, mined) {
                    return;
                }
                (fork.height, heavy)
            }
            Change::Finalize => {
                if let Some(boundary) = self.headers.finalizable() {
                    self.headers.finalize(boundary);
                }
                return;
            }
        };
        let path = self.builder.blocks(tip);
        let mined = &path[path.partition_point(|b| b.header().height <= fork)..];
        insert(&mut self.headers, mined).expect("valid headers");
        let now = self.headers.best().expect("verified").block;
        assert_eq!(now, tip, "sim: the heavier branch is best");
        if fork < best.height {
            self.tips.push(best.hash);
        }
    }

    fn publish(&mut self, context: &str) {
        let chain = Arc::new(self.headers.snapshot().expect("genesis verified"));
        self.given = Some(Arc::clone(&chain));
        self.step(Input::Chain(chain), context);
    }

    /// A fresh core, as the driver boots one: the chain, then every durable tip
    fn boot(&mut self, context: &str) {
        let enabled = self.writers.kinds();
        self.core = NfsCore::new(self.lookahead, enabled, WINDOW);
        self.told = enabled.iter().map(|kind| (kind, None)).collect();
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
                Output::Fetch(at) => self.ask(at, context),
                Output::Abandon(at) => self.abandon(at, context),
                Output::Fold { at, parent, block, covers } => {
                    self.fold(at, parent, &block, covers, context)
                }
                Output::Publish(Some(tip)) => self.served(tip, context),
                Output::Publish(None) => self.published = None,
            }
        }
        let wanted: HashSet<BlockHash> = self.core.wanted.values().copied().collect();
        assert_eq!(
            wanted, self.fetching,
            "{context}: one Fetch out per want, none past it: core's wants (left) vs Fetches out"
        );
        self.verify(context);
    }

    /// The balancer's checked answer, served within [`SERVED_WITHIN`]
    fn ask(&mut self, at: BlockRef, context: &str) {
        let fresh = self.fetching.insert(at.hash);
        assert!(fresh, "{context}: Fetch {at:?} twice while in flight");
        let honest = Block::clone(self.builder.block(at.hash));
        let checked = check_block(honest, at).expect("the asked block passes");
        let due = self.later(SERVED_WITHIN);
        self.pending.push((due, Due::Body(checked)));
    }

    /// - its fetch dropped
    /// - answer still lands half the time (a stale body: ignored)
    fn abandon(&mut self, at: BlockRef, context: &str) {
        assert!(self.fetching.remove(&at.hash), "{context}: Abandon {at:?} with no Fetch out");
        if self.random(2) == 0 {
            let answer = |due: &Due| matches!(due, Due::Body(body) if body.block_ref() == at);
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
        covers: IndexSet,
        context: &str,
    ) {
        let best = self.given.as_ref().map(|chain| chain.best().height);
        let at_height = at.height;
        let told = self.told_heights();
        assert!(
            self.active(),
            "{context}: Fold at {at_height:?} only inside the window {WINDOW} (N5): durable \
             {told:?}, best {best:?}"
        );
        let expected = (at.height != H0).then(|| self.oracle(block.header().prev_hash));
        let mut toys = Toys::default();
        for (kind, &durable) in told.iter() {
            let index = kind.name();
            let below = durable < Some(at.height);
            assert_eq!(
                covers.contains(kind),
                below,
                "{context}: Fold at {at_height:?} must cover {index} iff durable below it (N2): \
                 durable {durable:?}, covers {covers:?}"
            );
            if !below {
                continue;
            }
            let parent = match parent.as_ref().and_then(|parent| parent.get(kind).copied()) {
                Some(parent) => Some(parent),
                None => {
                    let at_parent =
                        durable == u32::from(at.height).checked_sub(1).map(BlockHeight::from);
                    assert!(
                        at_parent,
                        "{context}: Fold at {at_height:?}, {index} with no parent node folds on \
                         its durable tip exactly below (N2): durable {durable:?}"
                    );
                    expected
                }
            };
            assert_eq!(
                parent, expected,
                "{context}: Fold at {at_height:?}, {index} folds on the oracle's parent (N6): \
                 handed (left) vs oracle (right)"
            );
            toys.insert(kind, fold(parent, block));
        }
        let due = self.later(4);
        self.pending.push((due, Due::Folded { at, covers, toys }));
    }

    /// - N4: on its chain's best, every index = the oracle (node) or durable at or past it
    /// - N7: `at` of every node, the root and the decoy = the naive answer
    fn served(&mut self, tip: Publication<Toys>, context: &str) {
        let at = tip.tip;
        let best = tip.chain.best();
        assert!(
            tip.chain.on_best(at),
            "{context}: published tip on its chain's best (N4): published {at:?}, best {best:?}"
        );
        let toys = tip.graph.at(&tip.chain, tip.root, &at.hash).and_then(|base| base.folded);
        let expected = Some(self.oracle(at.hash));
        for (kind, &told) in self.told_heights().iter() {
            let durable = told >= Some(at.height);
            let folded = toys.as_ref().and_then(|toys| toys.get(kind).copied());
            assert!(
                durable || folded == expected,
                "{context}: published {:?}, {} = the oracle there or durable at or past it (N4): \
                 folded {folded:?}, oracle {expected:?}, durable {told:?}",
                at.height,
                kind.name(),
            );
        }
        let nodes = tip.graph.nodes().map(|node| node.at.hash);
        let asked: Vec<BlockHash> =
            nodes.chain(tip.root.map(|root| root.hash)).chain([self.decoy]).collect();
        for hash in asked {
            let got = tip.graph.at(&tip.chain, tip.root, &hash);
            let got = got.map(|base| (base.at, base.on_best, base.folded.is_some()));
            let expected = self.at(&tip, hash);
            assert_eq!(got, expected, "{context}: N7 at {hash:?}");
        }
        self.published = Some(at);
    }

    /// Naive `at`
    ///
    /// - root = unfolded
    /// - node = folded, on best iff its whole path is
    fn at(&self, tip: &Publication<Toys>, hash: BlockHash) -> Option<(BlockRef, bool, bool)> {
        if let Some(root) = tip.root.filter(|root| root.hash == hash) {
            return Some((root, true, false));
        }
        tip.graph.get(&hash)?;
        let blocks = self.builder.blocks(self.builder.block(hash).block_ref());
        let path: Vec<BlockRef> = blocks.iter().map(|block| block.block_ref()).collect();
        let on_best = path.iter().all(|at| tip.chain.on_best(*at));
        Some((path[path.len() - 1], on_best, true))
    }

    /// After every step: every node's folds = the oracle, no node outside the window
    fn verify(&mut self, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any step"));
        let (served, best) = (self.published, chain.best());
        assert!(
            served.is_none_or(|tip| chain.on_best(tip)),
            "{context}: last published tip still on best (N4): published {served:?}, best {best:?}"
        );
        let nodes: Vec<(BlockRef, Toys)> =
            self.core.graph.nodes().map(|node| (node.at, (*node.folded).clone())).collect();
        let heights: Vec<BlockHeight> = nodes.iter().map(|(at, _)| at.height).collect();
        assert!(
            nodes.is_empty() || self.active(),
            "{context}: no nodes outside the window {WINDOW} (N5): nodes {heights:?}, durable \
             {:?}, best {:?}",
            self.told_heights(),
            best.height,
        );
        for (at, toys) in nodes {
            let toy = Some(self.oracle(at.hash));
            let wrong = toys.iter().any(|(_, folded)| Some(*folded) != toy);
            assert!(
                !wrong,
                "{context}: node at {:?} = the oracle in every index it folds (N6): folded \
                 {toys:?}, oracle {toy:?}",
                at.height,
            );
        }
    }

    /// Each writer behind the finalized tip, no commit due: one scheduled after its delay
    fn schedule_commits(&mut self) {
        let finalized_tip = self.given.as_ref().map(|chain| chain.finalized_tip());
        let mut starting = Vec::new();
        for (kind, writer) in self.writers.iter_mut() {
            let behind = finalized_tip.map(|tip| tip.height) > writer.durable.map(|tip| tip.height);
            if behind && !writer.committing {
                writer.committing = true;
                starting.push((kind, writer.delay));
            }
        }
        for (kind, delay) in starting {
            let due = self.later(delay);
            self.pending.push((due, Due::Commit(kind)));
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
            let below = u64::try_from(due.len()).expect("usize = u64 (64-bit targets only)");
            let pick = due[usize::try_from(self.random(below)).expect("< due.len()")];
            let input = match self.pending.swap_remove(pick).1 {
                Due::Body(body) => {
                    let at = body.block_ref();
                    if self.core.wanted.get(&at.height) == Some(&at.hash) {
                        self.fetching.remove(&at.hash);
                    }
                    Input::Body(body)
                }
                Due::Folded { at, covers, toys } => {
                    Input::Folded { at, covers, folded: Arc::new(toys) }
                }
                Due::Commit(kind) => {
                    let finalized_tip = self.given.as_ref().map(|chain| chain.finalized_tip());
                    let writer = self.writers.get_mut(kind).expect("commits due = writers'");
                    writer.committing = false;
                    if finalized_tip.map(|tip| tip.height) > writer.durable.map(|tip| tip.height) {
                        writer.durable = finalized_tip;
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
    /// - `wipe` = that index's store too
    fn restart(&mut self, reset: bool, wipe: Option<usize>, context: &str) {
        if reset {
            // re-anchored like HeaderSync: one header `DEPTH` below best, nothing beneath (may
            // sit above every durable tip: the finalized path closes that gap by height)
            let path = self.builder.blocks(self.headers.best().expect("anchored").block);
            let depth = usize::try_from(DEPTH.get()).expect("u32 fits usize (64-bit targets only)");
            let anchor = path.len().saturating_sub(1 + depth);
            let header = path[anchor].header();
            let decoded = decode_header(&encode_header(header)).expect("MockChain headers decode");
            self.headers = HeaderChain::new(DEPTH);
            self.headers.anchor(&decoded, header.height).expect("MockChain nBits valid");
            insert(&mut self.headers, &path[anchor + 1..]).expect("the best path verifies");
        }
        if let Some((_, wiped)) = wipe.and_then(|at| self.writers.iter_mut().nth(at)) {
            wiped.durable = None;
        }
        for (_, writer) in self.writers.iter_mut() {
            writer.committing = false;
        }
        self.pending.clear();
        self.fetching.clear();
        self.published = None;
        self.boot(context);
    }

    /// Liveness: every index durable through the finalized tip, served at best
    fn settle(&mut self) {
        // finalized past every durable tip (a reset header store may sit below them)
        self.change(&Change::Extend(DEPTH.get()));
        self.change(&Change::Finalize);
        self.publish("settle");
        let chain = Arc::clone(self.given.as_ref().expect("published"));
        let best = chain.best();
        for second in 0..SETTLE {
            let finalized = Some(chain.finalized_tip());
            let durable = self.writers.iter().all(|(_, writer)| writer.durable == finalized);
            if durable && self.published == Some(best) {
                return;
            }
            self.advance(&format!("settle {second}s"));
        }
        let durable = self.writers.iter().map(|(kind, writer)| (kind, writer.durable));
        let durable: PerIndex<Option<BlockHeight>> =
            durable.map(|(kind, tip)| (kind, tip.map(|tip| tip.height))).collect();
        panic!(
            "liveness: within {SETTLE} s every index durable through finalized {:?} and best \
             {best:?} served: durable {durable:?}, served {:?}",
            chain.finalized_tip().height,
            self.published,
        );
    }
}

fn run(moves: &[Move], delays: &[u64], lookahead: usize, seed: u64) {
    let mut builder = MockChain::regtest().varied_work();
    let genesis = builder.genesis();
    let decoy = builder.branch(genesis).mine_empty(1).tip().hash;
    let headers = HeaderChain::anchored(&builder, DEPTH);
    // `seed`-rotated kinds: sets past a prefix of `IndexKind::ALL` (saved cases' draws unchanged)
    let first = (seed % IndexKind::ALL.len() as u64) as usize;
    let kinds = IndexKind::ALL.into_iter().cycle().skip(first);
    let writer = |delay| Writer { durable: None, delay, committing: false };
    let writers: PerIndex<Writer> =
        kinds.zip(delays).map(|(kind, &delay)| (kind, writer(delay))).collect();
    let mut sim = Sim {
        builder,
        headers,
        decoy,
        lookahead,
        core: NfsCore::new(lookahead, writers.kinds(), WINDOW),
        given: None,
        pending: Vec::new(),
        fetching: HashSet::new(),
        writers,
        told: PerIndex::default(),
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
