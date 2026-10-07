//! [`NfsCore`] against naive writers and an oracle fold (`nfs.md` §10), `check()` after every step
//!
//! - Chain moves (a real [`HeaderChain`], work varying per branch): extend, reorg at a random depth
//!   above final onto a heavier branch (longer, same height, or a retreat), an earlier best made
//!   heaviest again (nodes reused), finalize
//! - Each move published or coalesced with the next (a `watch` keeps the latest)
//! - Sources: honest, slow (past [`HEDGE`]), wrong block, poisoned, mutated, failing, silent
//! - Source 0 honest or slow (one source holding every block)
//! - Folds answered after a random delay, in random order
//! - Each index commits after its own random delay
//! - Restarts: fresh core from the writers' durable tips (`reset` = header store lost too)
//! - Oracle ([`Toy`]) = fold from genesis along each block's own path

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use zaino_header_chain::{HeaderChain, Record, Rejected, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};

use super::{Final, Input, NfsCore, Output, SnapshotTip};
use crate::fetch::{check_block, Answer, HEDGE};
use crate::graph::on_best;
use crate::snapshot::Branch;

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const SOURCES: usize = 4;
const INDEXES: usize = 3;
/// Virtual seconds a case may take to settle once the moves end
const SETTLE: u32 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Honest,
    Slow,
    WrongBlock,
    Poisoned,
    Mutated,
    Failing,
    Silent,
}

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
    Restart { reset: bool },
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
        1 => any::<bool>().prop_map(|reset| Move::Restart { reset }),
    ];
    prop::collection::vec(one, 1..32)
}

fn kinds() -> impl Strategy<Value = [Kind; SOURCES]> {
    let any = prop_oneof![
        Just(Kind::Honest),
        Just(Kind::Slow),
        Just(Kind::WrongBlock),
        Just(Kind::Poisoned),
        Just(Kind::Mutated),
        Just(Kind::Failing),
        Just(Kind::Silent),
    ];
    let first = prop_oneof![Just(Kind::Honest), Just(Kind::Slow)];
    (first, [any.clone(), any.clone(), any]).prop_map(|(first, [a, b, c])| [first, a, b, c])
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// - `check()` after every step
    /// - Each index: the best path's final prefix, once, in order, = the oracle
    /// - Published tip folded on the best; no node sent folded pruned before every index holds it
    /// - Settled: every index durable through the final tip, served tip = best
    #[test]
    fn the_final_stream_and_served_tip_follow_the_verified_best_through_reorgs_lies_and_restarts(
        moves in moves(),
        kinds in kinds(),
        sources in 1usize..=SOURCES,
        delays in prop::collection::vec(0u64..=20, 1..=INDEXES),
        lookahead in 1usize..=4,
        seed in any::<u64>(),
    ) {
        run(&moves, &kinds[..sources], &delays, lookahead, seed);
    }
}

/// Toy fold payload: height + running digest of every hash on the path
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Toy {
    height: Height,
    digest: u64,
}

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
    Body { from: usize, at: BlockRef, answer: Answer },
    Folded { at: BlockRef, toy: Toy },
    Commit { index: usize, len: usize },
}

struct Sim<'a> {
    builder: Chain,
    headers: HeaderChain,
    decoy: BlockHash,
    kinds: &'a [Kind],
    lookahead: usize,
    core: NfsCore<Toy>,
    given: Option<Arc<VerifiedChain>>,
    pending: Vec<(Instant, Due)>,
    writers: Vec<Writer>,
    published: Option<BlockRef>,
    sent_folded: BTreeMap<Height, BlockHash>,
    tips: Vec<BlockHash>,
    oracle: HashMap<BlockHash, Toy>,
    now: Instant,
    rng: u64,
}

impl Sim<'_> {
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

    fn step(&mut self, input: Input<Toy>, context: &str) {
        let outputs = self.core.step(input, self.now).expect("durable tips stay on the chain");
        self.core.check();
        for output in outputs {
            match output {
                Output::Fetch { from, height, record } => self.ask(from, height, record),
                Output::Misanswered { .. } | Output::Unserved { .. } => {}
                Output::Fold { at, parent, block } => self.fold(at, parent, &block, context),
                Output::Send(block) => self.send(block, context),
                Output::Publish(tip) => self.served(tip, context),
            }
        }
        self.verify(context);
    }

    /// Source's answer, through the real [`check_block`] (a lie never passes it)
    fn ask(&mut self, from: usize, height: Height, record: Record) {
        let kind = self.kinds[from];
        let honest = self.builder.block(record.hash).clone();
        let header = honest.header().clone();
        let served = match kind {
            Kind::Honest | Kind::Slow => Some(honest),
            Kind::WrongBlock => Some(self.builder.block(self.decoy).clone()),
            Kind::Poisoned => {
                let extra = self.builder.block(self.decoy).transactions()[0].clone();
                Some(Block::new(header, [honest.transactions().to_vec(), vec![extra]].concat()))
            }
            Kind::Mutated => {
                let txs = honest.transactions();
                Some(Block::new(header, [txs, txs].concat()))
            }
            Kind::Failing => None,
            Kind::Silent => return,
        };
        let answer = match served.map(|block| check_block(block, height, &record)) {
            Some(Ok(checked)) => Answer::Checked(checked),
            Some(Err(why)) => Answer::Misanswered(why),
            None => Answer::Failed,
        };
        let honest = matches!(kind, Kind::Honest | Kind::Slow);
        assert_eq!(matches!(answer, Answer::Checked(_)), honest, "N1: {kind:?} caught by check");
        let due = match kind {
            Kind::Slow => self.later(4) + HEDGE + Duration::from_secs(1),
            _ => self.later(3),
        };
        let at = BlockRef { hash: record.hash, height };
        self.pending.push((due, Due::Body { from, at, answer }));
    }

    /// Parent = the oracle's (a node) or every index durable exactly below `at` (the root)
    fn fold(&mut self, at: BlockRef, parent: Option<Arc<Toy>>, block: &Block, context: &str) {
        let prev = block.header().prev_hash;
        let parent = match parent {
            Some(parent) => {
                let expected = self.oracle(prev);
                assert_eq!(*parent, expected, "{context}: N6 Fold {at:?} on a wrong parent");
                Some(*parent)
            }
            None => {
                let below = at.height.checked_sub(1);
                for (index, writer) in self.writers.iter().enumerate() {
                    let durable = writer.tip().map(|tip| tip.height);
                    let lockstep = durable == below && writer.applied.len() == writer.durable;
                    assert!(lockstep, "{context}: N3 Fold {at:?} on the root before index {index}");
                }
                self.writers[0].applied.last().map(|(_, toy)| *toy)
            }
        };
        let toy = fold(parent, block);
        let due = self.later(4);
        self.pending.push((due, Due::Folded { at, toy }));
    }

    /// Every index: each height once, ascending, final, on the best; folded = the oracle
    fn send(&mut self, block: Final<Toy>, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any send"));
        let header = block.block.header();
        let at = BlockRef { hash: header.hash, height: header.height };
        assert!(on_best(&chain, at), "{context}: N5 Send {at:?} off the verified best");
        let final_tip = chain.final_tip().map(|tip| tip.height);
        assert!(Some(at.height) <= final_tip, "{context}: N5 Send {at:?} above the final tip");
        let expected = self.oracle(at.hash);
        if let Some(folded) = &block.folded {
            assert_eq!(**folded, expected, "{context}: N6 Send {at:?} folded wrong");
            self.sent_folded.insert(at.height, at.hash);
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
            let toy =
                block.folded.as_deref().copied().unwrap_or_else(|| fold(parent, &block.block));
            assert_eq!(toy, expected, "{context}: N6 index {index} at {at:?}");
            writer.applied.push((at.hash, toy));
            let len = writer.applied.len();
            let delay = writer.delay;
            let due = self.later(delay);
            self.pending.push((due, Due::Commit { index, len }));
        }
    }

    /// - N4: on its chain's best; folded = the oracle, unfolded = durable in every index
    /// - G7: `at` of every node, the root, a block below it and the decoy = the naive answer
    fn served(&mut self, tip: SnapshotTip<Toy>, context: &str) {
        let at = tip.tip;
        assert!(on_best(&tip.chain, at), "{context}: N4 served {at:?} off its best");
        let served = tip.graph.at(&tip.chain, tip.root, &at.hash);
        match served.and_then(|served| served.folded) {
            Some(folded) => {
                let expected = self.oracle(at.hash);
                assert_eq!(*folded, expected, "{context}: N4 served {at:?} folded wrong");
            }
            None => {
                assert_eq!(tip.root, Some(at), "{context}: N4 served {at:?} = a node or the root");
                let durable = self
                    .writers
                    .iter()
                    .all(|writer| writer.durable > u32::from(at.height) as usize);
                assert!(durable, "{context}: N4 served {at:?} unfolded and not durable");
            }
        }
        let below = tip.root.and_then(|root| tip.chain.hash_at(root.height.checked_sub(1)?));
        let nodes = tip.graph.nodes().map(|node| node.at.hash);
        let asked: Vec<BlockHash> =
            nodes.chain(tip.root.map(|root| root.hash)).chain(below).chain([self.decoy]).collect();
        for hash in asked {
            let got = tip.graph.at(&tip.chain, tip.root, &hash);
            let got = got.map(|base| (base.at, base.branch, base.folded.map(|folded| *folded)));
            let expected = self.at(&tip, hash);
            assert_eq!(got, expected, "{context}: G7 at {hash:?}");
        }
        self.published = Some(at);
    }

    /// Naive `at`: the root unfolded; a node = its oracle fold, branch = its path vs the chain's
    fn at(
        &mut self,
        tip: &SnapshotTip<Toy>,
        hash: BlockHash,
    ) -> Option<(BlockRef, Branch, Option<Toy>)> {
        if let Some(root) = tip.root.filter(|root| root.hash == hash) {
            return Some((root, Branch::Best, None));
        }
        if !tip.graph.contains(&hash) {
            return None;
        }
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
        Some((path[path.len() - 1], branch, Some(self.oracle(hash))))
    }

    /// After every step: the served tip on the best, every node = the oracle, every node sent
    /// folded held until every index holds it
    fn verify(&mut self, context: &str) {
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any step"));
        let served = self.published;
        assert!(served.is_none_or(|tip| on_best(&chain, tip)), "{context}: N4 served off best");
        let nodes: Vec<(BlockRef, Toy)> =
            self.core.graph.nodes().map(|node| (node.at, *node.folded)).collect();
        for (at, toy) in nodes {
            assert_eq!(toy, self.oracle(at.hash), "{context}: N6 node {at:?}");
        }
        let root = self.writers.iter().map(|writer| writer.durable).min().unwrap_or(0);
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
                Due::Body { from, at, answer } => Input::Body { from, at, answer },
                Due::Folded { at, toy } => Input::Folded { at, folded: Arc::new(toy) },
                Due::Commit { index, len } => {
                    let writer = &mut self.writers[index];
                    writer.durable = writer.durable.max(len);
                    Input::Durable { index, tip: writer.tip() }
                }
            };
            self.step(input, context);
        }
    }

    /// One virtual second: a tick, then everything due
    fn advance(&mut self, context: &str) {
        self.now += Duration::from_secs(1);
        if self.given.is_some() {
            self.step(Input::Tick, context);
        }
        self.answer(context);
    }

    /// Everything not durable lost: in-flight fetches, folds, writer buffers, the core
    fn restart(&mut self, reset: bool, context: &str) {
        if reset {
            let genesis = self.builder.genesis().hash;
            let tip = *self.best_path().last().expect("genesis");
            self.headers = HeaderChain::regtest_in_memory(genesis, DEPTH);
            self.headers.insert_blocks(&self.builder.path(tip)).expect("the best path verifies");
        }
        for writer in &mut self.writers {
            writer.applied.truncate(writer.durable);
        }
        let durable = self.writers.iter().map(Writer::tip).collect();
        self.core = NfsCore::new(self.kinds.len(), self.lookahead, durable);
        self.pending.clear();
        self.published = None;
        self.sent_folded.clear();
        self.publish(context);
    }

    /// Liveness: every index durable through the final tip, the served tip = best
    fn settle(&mut self) {
        // final past every durable tip (a reset header store may sit below them)
        self.change(&Change::Extend(DEPTH.get()));
        self.change(&Change::Finalize);
        self.publish("settle");
        let path = self.best_path();
        let chain = Arc::clone(self.given.as_ref().expect("published"));
        let final_len = chain.final_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let best = Some(chain.best());
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

fn run(moves: &[Move], kinds: &[Kind], delays: &[u64], lookahead: usize, seed: u64) {
    let mut builder = Chain::new();
    let genesis = builder.genesis().hash;
    let decoy = builder.mine(genesis).hash;
    let mut headers = HeaderChain::regtest_in_memory(genesis, DEPTH);
    headers.insert_blocks(&builder.path(genesis)).expect("genesis");
    let writers: Vec<Writer> =
        delays.iter().map(|&delay| Writer { applied: Vec::new(), durable: 0, delay }).collect();
    let mut sim = Sim {
        builder,
        headers,
        decoy,
        kinds,
        lookahead,
        core: NfsCore::new(kinds.len(), lookahead, vec![None; writers.len()]),
        given: None,
        pending: Vec::new(),
        writers,
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
            Move::Restart { reset } => sim.restart(*reset, &context),
        }
    }
    sim.settle();
}
