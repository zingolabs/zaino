//! [`NfsCore`] against naive writers and an oracle fold (`nfs.md` §10), `check()` after every step
//!
//! - Chain moves (a real [`HeaderChain`], work varying per branch): extend, reorg at a random depth
//!   above final onto a heavier branch (longer, same height, or a retreat), an earlier best made
//!   heaviest again (nodes reused), finalize
//! - Each move published or coalesced with the next (a `watch` keeps the latest)
//! - Fetches answered late (random delay), out of order, or after their abandon (a stale body);
//!   who answers, lies, hedges, retries = `zaino-traffic`'s model (every answer here checked)
//! - Folds answered after a random delay, in random order
//! - Each index commits after its own random delay
//! - Each send delivered within the slowest index's delay
//! - Restarts: fresh core from the writers' durable tips (`reset` = header store lost too,
//!   `wipe` = one index's directory deleted: enabled late, it lags until durable at the root)
//! - Indexes 0 + 1 one group when `grouped` (value-balance + compact-block: they join together)
//! - Oracle ([`Toy`]) = fold from genesis along each block's own path, per index

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use zaino_header_chain::{HeaderChain, Record, Rejected, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_traffic::Urgency;

use super::{Final, Indexes, Input, NfsCore, Output, SnapshotTip};
use crate::fetch::{check_block, Checked};
use crate::graph::on_best;
use crate::snapshot::Branch;

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const INDEXES: usize = 3;
/// Most virtual seconds the balancer takes to serve a body (hedges, retries, a bench)
const SERVED_WITHIN: u64 = 20;
/// Virtual seconds a case may take to settle once the moves end
const SETTLE: u32 = 20_000;

/// - `Reorg` = top `depth` replaced by `len` heavier blocks (`len` < `depth` = a retreat)
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
    /// - One `Fetch` per want until its body or its `Abandon`; `Tip` iff above the final tip
    /// - Each index: the best path's final prefix, once, in order, = the oracle (folded for it
    ///   by the NFS, else by itself)
    /// - Never unfolded after folded
    /// - Folds: the joined indexes, each on its own oracle parent (the root: each durable there)
    /// - Published tip folded on the best; each index it serves = the oracle (the root: durable
    ///   at or past it); no node sent folded pruned before every joined index holds it
    /// - Settled: every index joined, durable through the final tip, served at best
    #[test]
    fn the_final_stream_and_served_tip_follow_the_verified_best_through_reorgs_restarts_and_late_indexes(
        moves in moves(),
        delays in prop::collection::vec(0u64..=20, 1..=INDEXES),
        grouped in any::<bool>(),
        lookahead in 1usize..=4,
        seed in any::<u64>(),
    ) {
        run(&moves, &delays, grouped, lookahead, seed);
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

/// Index writer: `applied[h]` = block + payload at `h`, `durable` = blocks committed
#[derive(Debug)]
struct Writer {
    applied: Vec<(BlockHash, Toy)>,
    durable: usize,
    delay: u64,
}

impl Writer {
    fn tip(&self) -> Option<BlockRef> {
        let at = self.durable.checked_sub(1)?;
        Some(BlockRef { hash: self.applied[at].0, height: height(at as u32) })
    }
}

enum Due {
    Body(Checked),
    Folded { at: BlockRef, toys: Toys },
    Commit { index: usize, len: usize },
    Delivered,
}

/// - `fetching` = blocks with a `Fetch` out, neither answered nor abandoned
/// - `decoy` = a block off every chain (G7: `at` of it = `None`)
/// - `stream_folded` = a folded step sent since the last restart
/// - `published` = last published tip + the indexes it serves
struct Sim {
    builder: Chain,
    headers: HeaderChain,
    decoy: BlockHash,
    lookahead: usize,
    groups: Vec<Indexes>,
    core: NfsCore<Toys>,
    given: Option<Arc<VerifiedChain>>,
    pending: Vec<(Instant, Due)>,
    fetching: HashSet<BlockHash>,
    writers: Vec<Writer>,
    stream_folded: bool,
    published: Option<(BlockRef, Indexes)>,
    sent_folded: BTreeMap<Height, BlockHash>,
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
        let block = self.builder.block(hash).clone();
        let header = block.header();
        let parent = (header.height != Height::GENESIS).then(|| self.oracle(header.prev_hash));
        let toy = fold(parent, &block);
        self.oracle.insert(hash, toy);
        toy
    }

    fn best_path(&self) -> Vec<BlockHash> {
        let chain = self.headers.verified().expect("genesis verified");
        let best = u32::from(chain.best().height);
        (0..=best).map(|h| chain.hash_at(height(h)).expect("best path")).collect()
    }

    fn durable_floor(&self) -> u32 {
        self.writers.iter().map(|writer| writer.durable).max().unwrap_or(0).saturating_sub(1) as u32
    }

    fn change(&mut self, change: &Change) {
        let best = self.headers.best().expect("genesis verified").block;
        // reset header store forgets finality (durable tips still bound a fork)
        let final_height = self.headers.final_tip().map_or(0, |tip| u32::from(tip.height));
        let floor = final_height.max(self.durable_floor());
        let (fork, tip) = match *change {
            Change::Extend(count) => (best.height, self.builder.extend(best.hash, count).hash),
            Change::Reorg { depth, len } => {
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let path = self.best_path();
                let (parent, replaced) = (path[at as usize], &path[at as usize + 1..]);
                // nested reorgs past the u128 work range: skipped
                let Some(heavy) = self.builder.mine_heavier(parent, replaced) else { return };
                (height(at), self.builder.extend(heavy.hash, len - 1).hash)
            }
            Change::Revive { pick } => {
                let Some(&old) = self.tips.get(usize::from(pick) % self.tips.len().max(1)) else {
                    return;
                };
                let path = self.best_path();
                let branch = self.builder.path(old);
                let shared = branch.iter().zip(&path).take_while(|(b, p)| b.header().hash == **p);
                let fork = shared.count() - 1;
                if (fork as u32) < floor || fork + 1 == branch.len() {
                    return;
                }
                let Some(heavy) = self.builder.mine_heavier(old, &path[fork + 1..]) else { return };
                let mined = &self.builder.path(heavy.hash)[fork + 1..];
                // a long side branch re-entering past the header chain's side bound: evicted (H4)
                if let Err(Rejected::Orphan) = self.headers.insert_blocks(mined) {
                    return;
                }
                (height(fork as u32), heavy.hash)
            }
            Change::Finalize => {
                if let Some(boundary) = self.headers.finalizable() {
                    self.headers.finalize(boundary).expect("in-memory store");
                }
                return;
            }
        };
        let mined = &self.builder.path(tip)[u32::from(fork) as usize + 1..];
        self.headers.insert_blocks(mined).expect("valid headers");
        let now = self.headers.best().expect("verified").block.hash;
        assert_eq!(now, tip, "sim: the heavier branch is best");
        if fork < best.height {
            self.tips.push(best.hash);
        }
    }

    fn publish(&mut self, context: &str) {
        let chain = Arc::new(self.headers.verified().expect("genesis verified"));
        for (index, writer) in self.writers.iter().enumerate() {
            let tip = writer.tip();
            let held = tip.is_none_or(|tip| on_best(&chain, tip));
            assert!(held, "{context}: N5 index {index} durable {tip:?} off the verified chain");
        }
        self.given = Some(Arc::clone(&chain));
        self.step(Input::Chain(chain), context);
    }

    fn step(&mut self, input: Input<Toys>, context: &str) {
        let outputs = self.core.step(input).expect("durable tips stay on the chain");
        self.core.check();
        for output in outputs {
            match output {
                Output::Fetch { at, record, urgency } => self.ask(at, record, urgency, context),
                Output::Abandon(at) => self.abandon(at, context),
                Output::Fold { at, parent, block, covers } => {
                    self.fold(at, parent, &block, covers, context)
                }
                Output::Send(block) => self.send(block, context),
                Output::Publish(tip) => self.served(tip, context),
            }
        }
        let wanted: HashSet<BlockHash> = self.core.wanted.values().copied().collect();
        assert_eq!(wanted, self.fetching, "{context}: one Fetch out per want, none past it");
        self.verify(context);
    }

    /// The balancer's checked answer, served within [`SERVED_WITHIN`]
    fn ask(&mut self, at: BlockRef, record: Record, urgency: Urgency, context: &str) {
        let chain = self.given.as_ref().expect("a chain before any fetch");
        let final_height = chain.final_tip().map(|tip| tip.height);
        let tip = Some(at.height) > final_height;
        assert_eq!(urgency == Urgency::Tip, tip, "{context}: Fetch {at:?} {urgency:?}");
        assert_eq!(record.hash, at.hash, "{context}: Fetch {at:?} with another's header");
        let fresh = self.fetching.insert(at.hash);
        assert!(fresh, "{context}: Fetch {at:?} twice while in flight");
        let honest = self.builder.block(at.hash).clone();
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

    /// Each joined index (`covers`): on the oracle's parent (a node folding exactly `covers`), or
    /// durable exactly below `at` (the root)
    fn fold(
        &mut self,
        at: BlockRef,
        parent: Option<Arc<Toys>>,
        block: &Block,
        covers: Indexes,
        context: &str,
    ) {
        assert_eq!(covers, self.core.joined, "{context}: J3 Fold {at:?} of the joined indexes");
        let expected =
            (at.height != Height::GENESIS).then(|| self.oracle(block.header().prev_hash));
        let mut toys: Toys = vec![None; self.writers.len()];
        for (index, toy) in toys.iter_mut().enumerate().filter(|(index, _)| covers.contains(*index))
        {
            let parent = match &parent {
                Some(parent) => {
                    let parent = parent[index];
                    assert_eq!(parent, expected, "{context}: N6 Fold {at:?} index {index} parent");
                    parent
                }
                None => {
                    let writer = &self.writers[index];
                    let durable = writer.tip().map(|tip| tip.height);
                    let lockstep = durable == at.height.checked_sub(1)
                        && writer.applied.len() == writer.durable;
                    assert!(lockstep, "{context}: N3 Fold {at:?} on the root before index {index}");
                    writer.applied.last().map(|(_, toy)| *toy)
                }
            };
            *toy = Some(fold(parent, block));
        }
        if let Some(parent) = &parent {
            let only =
                (0..toys.len()).all(|index| parent[index].is_some() == covers.contains(index));
            assert!(only, "{context}: J4 Fold {at:?} on a parent folding other indexes");
        }
        let due = self.later(4);
        self.pending.push((due, Due::Folded { at, toys }));
    }

    /// Every index: each height once, ascending, final, on the best; folded = the oracle, an
    /// index it lacks folding itself; never unfolded after folded
    fn send(&mut self, block: Final<Toys>, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any send"));
        let header = block.block.header();
        let at = BlockRef { hash: header.hash, height: header.height };
        assert!(on_best(&chain, at), "{context}: N5 Send {at:?} off the verified best");
        let final_tip = chain.final_tip().map(|tip| tip.height);
        assert!(Some(at.height) <= final_tip, "{context}: N5 Send {at:?} above the final tip");
        let expected = self.oracle(at.hash);
        match &block.folded {
            Some(toys) => {
                let wrong = toys.iter().flatten().any(|toy| *toy != expected);
                assert!(!wrong, "{context}: N6 Send {at:?} folded wrong");
                self.stream_folded = true;
                self.sent_folded.insert(at.height, at.hash);
            }
            None => {
                let after = self.stream_folded;
                assert!(!after, "{context}: N5 Send {at:?} unfolded after a folded step");
            }
        }
        let position = u32::from(at.height) as usize;
        for index in 0..self.writers.len() {
            let writer = &mut self.writers[index];
            if position < writer.applied.len() {
                let replay = position < writer.durable && writer.applied[position].0 == at.hash;
                assert!(replay, "{context}: N5 Send {at:?} twice to index {index}");
                continue;
            }
            assert_eq!(position, writer.applied.len(), "{context}: N5 Send {at:?} out of order");
            let parent = writer.applied.last().map(|(_, toy)| *toy);
            let sent = block.folded.as_deref().and_then(|toys| toys[index]);
            let toy = sent.unwrap_or_else(|| fold(parent, &block.block));
            assert_eq!(toy, expected, "{context}: N6 index {index} at {at:?}");
            writer.applied.push((at.hash, toy));
            let len = writer.applied.len();
            let delay = writer.delay;
            let due = self.later(delay);
            self.pending.push((due, Due::Commit { index, len }));
        }
        let slowest = self.writers.iter().map(|writer| writer.delay).max().unwrap_or(0);
        let due = self.later(slowest);
        self.pending.push((due, Due::Delivered));
    }

    /// - N4: on its chain's best; folded = the oracle per index it serves, unfolded = durable in
    ///   every index it serves (J3: a lagging index never served)
    /// - G7: `at` of every node, the root, a block below it and the decoy = the naive answer
    fn served(&mut self, tip: SnapshotTip<Toys>, context: &str) {
        let at = tip.tip;
        assert!(on_best(&tip.chain, at), "{context}: N4 served {at:?} off its best");
        let served = tip.graph.at(&tip.chain, tip.root, &at.hash);
        let covers = match served.and_then(|served| served.folded) {
            Some(toys) => {
                let expected = Some(self.oracle(at.hash));
                let covers = (0..toys.len()).filter(|index| toys[*index].is_some());
                let covers = covers.fold(Indexes::default(), |all, at| all.union(Indexes::one(at)));
                for index in covers.iter() {
                    let toy = toys[index];
                    assert_eq!(toy, expected, "{context}: N4 served {at:?} index {index} wrong");
                }
                covers
            }
            None => {
                assert_eq!(tip.root, Some(at), "{context}: N4 served {at:?} = a node or the root");
                for index in tip.joined.iter() {
                    let durable = self.writers[index].durable > u32::from(at.height) as usize;
                    assert!(durable, "{context}: J3 served {at:?} index {index} not durable");
                }
                tip.joined
            }
        };
        assert!(tip.joined.covers(covers), "{context}: J3 served {at:?} past the joined");
        let below = tip.root.and_then(|root| tip.chain.hash_at(root.height.checked_sub(1)?));
        let nodes = tip.graph.nodes().map(|node| node.at.hash);
        let asked: Vec<BlockHash> =
            nodes.chain(tip.root.map(|root| root.hash)).chain(below).chain([self.decoy]).collect();
        for hash in asked {
            let got = tip.graph.at(&tip.chain, tip.root, &hash);
            let got =
                got.map(|base| (base.at, base.branch, base.folded.map(|toys| (*toys).clone())));
            let expected = self.at(&tip, hash);
            assert_eq!(got, expected, "{context}: G7 at {hash:?}");
        }
        self.published = Some((at, covers));
    }

    /// Naive `at`: the root unfolded; a node = its oracle fold per index it folds, branch = its
    /// path vs the chain's
    fn at(
        &mut self,
        tip: &SnapshotTip<Toys>,
        hash: BlockHash,
    ) -> Option<(BlockRef, Branch, Option<Toys>)> {
        if let Some(root) = tip.root.filter(|root| root.hash == hash) {
            return Some((root, Branch::Best, None));
        }
        let covers = tip.graph.get(&hash)?.covers;
        let path: Vec<BlockRef> = self
            .builder
            .path(hash)
            .iter()
            .map(|block| BlockRef { hash: block.header().hash, height: block.header().height })
            .collect();
        let shared = path.iter().take_while(|at| on_best(&tip.chain, **at)).count();
        let branch = match path.get(shared) {
            None => Branch::Best,
            Some(_) => Branch::Side { from: path[shared - 1] },
        };
        let toy = self.oracle(hash);
        let toys = (0..self.writers.len()).map(|index| covers.contains(index).then_some(toy));
        Some((path[path.len() - 1], branch, Some(toys.collect())))
    }

    /// After every step: the served tip on the best, every node = the oracle for each index it
    /// folds (and none other), every node sent folded held until every joined index holds it
    fn verify(&mut self, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any step"));
        let served = self.published.map(|(tip, _)| tip);
        assert!(served.is_none_or(|tip| on_best(&chain, tip)), "{context}: N4 served off best");
        let nodes: Vec<(BlockRef, Indexes, Toys)> = self
            .core
            .graph
            .nodes()
            .map(|node| (node.at, node.covers, (*node.folded).clone()))
            .collect();
        for (at, covers, toys) in nodes {
            let toy = self.oracle(at.hash);
            let expected: Toys =
                (0..toys.len()).map(|index| covers.contains(index).then_some(toy)).collect();
            assert_eq!(toys, expected, "{context}: N6 node {at:?} folding {covers:?}");
        }
        let joined = self.core.joined.iter().map(|index| self.writers[index].durable);
        let root = joined.min().unwrap_or(0);
        self.sent_folded = self.sent_folded.split_off(&height(root as u32));
        for (height, hash) in &self.sent_folded {
            let held = self.core.graph.contains(hash);
            assert!(held, "{context}: N3 node {height:?} pruned before every index holds it");
        }
    }

    /// Every body, fold and commit due by now, in a random order
    fn answer(&mut self, context: &str) {
        loop {
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
                Due::Folded { at, toys } => Input::Folded { at, folded: Arc::new(toys) },
                Due::Delivered => Input::Delivered,
                Due::Commit { index, len } => {
                    let writer = &mut self.writers[index];
                    writer.durable = writer.durable.max(len);
                    Input::Durable { index, tip: writer.tip() }
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

    /// - Everything not durable lost: in-flight fetches, folds, writer buffers, the core
    /// - `wipe` (mod the index count) = that index's store too
    fn restart(&mut self, reset: bool, wipe: Option<u8>, context: &str) {
        if reset {
            let genesis = self.builder.genesis().hash;
            let tip = *self.best_path().last().expect("genesis");
            self.headers = HeaderChain::regtest_in_memory(genesis, DEPTH);
            self.headers.insert_blocks(&self.builder.path(tip)).expect("the best path verifies");
        }
        if let Some(wipe) = wipe {
            let count = self.writers.len();
            self.writers[usize::from(wipe) % count].durable = 0;
        }
        for writer in &mut self.writers {
            writer.applied.truncate(writer.durable);
        }
        let durable = self.writers.iter().map(Writer::tip).collect();
        self.core = NfsCore::new(self.lookahead, durable, self.groups.clone());
        self.pending.clear();
        self.fetching.clear();
        self.stream_folded = false;
        self.published = None;
        self.sent_folded.clear();
        self.publish(context);
    }

    /// Liveness: every index joined, durable through the final tip, served at best
    fn settle(&mut self) {
        // final past every durable tip (a reset header store may sit below them)
        self.change(&Change::Extend(DEPTH.get()));
        self.change(&Change::Finalize);
        self.publish("settle");
        let path = self.best_path();
        let chain = Arc::clone(self.given.as_ref().expect("published"));
        let final_len = chain.final_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let best = Some((chain.best(), Indexes::first(self.writers.len())));
        for second in 0..SETTLE {
            let durable = self.writers.iter().all(|writer| {
                let hashes: Vec<BlockHash> = writer.applied.iter().map(|(hash, _)| *hash).collect();
                writer.durable == final_len && hashes == path[..final_len]
            });
            if durable && self.published == best {
                return;
            }
            self.advance(&format!("settle {second}s"));
        }
        let durable: Vec<usize> = self.writers.iter().map(|writer| writer.durable).collect();
        panic!(
            "liveness: never settled: durable {durable:?} of {final_len}, served {:?} of {best:?}",
            self.published
        );
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small chain")
}

fn run(moves: &[Move], delays: &[u64], grouped: bool, lookahead: usize, seed: u64) {
    let mut builder = Chain::new();
    let genesis = builder.genesis().hash;
    let decoy = builder.mine(genesis).hash;
    let mut headers = HeaderChain::regtest_in_memory(genesis, DEPTH);
    headers.insert_blocks(&builder.path(genesis)).expect("genesis");
    let writers: Vec<Writer> =
        delays.iter().map(|&delay| Writer { applied: Vec::new(), durable: 0, delay }).collect();
    let pair = grouped && writers.len() >= 2;
    let groups: Vec<Indexes> = match pair {
        true => [vec![Indexes::first(2)], (2..writers.len()).map(Indexes::one).collect()].concat(),
        false => (0..writers.len()).map(Indexes::one).collect(),
    };
    let mut sim = Sim {
        builder,
        headers,
        decoy,
        lookahead,
        core: NfsCore::new(lookahead, vec![None; writers.len()], groups.clone()),
        groups,
        given: None,
        pending: Vec::new(),
        fetching: HashSet::new(),
        writers,
        stream_folded: false,
        published: None,
        sent_folded: BTreeMap::new(),
        tips: Vec::new(),
        oracle: HashMap::new(),
        now: Instant::now(),
        rng: seed | 1,
    };
    sim.publish("start");
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
