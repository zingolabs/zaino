//! [`ProducerCore`] against a naive index (`verified-chain.md` §10 layer 2): random verified-chain
//! evolutions, random sources, `check()` after every step
//!
//! - Chain moves (a real [`HeaderChain`], work varying per branch): extend, reorg at a random depth
//!   above final onto a heavier branch (longer, same height, or a retreat), finalize the
//!   boundary; a move published or coalesced with the next (a `watch` keeps the latest)
//! - Sources: honest, slow (past [`HEDGE`]), wrong block, poisoned body, mutated body, failing,
//!   silent; source 0 honest or slow (P5: one source holding every block)
//! - Restarts: a fresh core from the index's durable tip (+ a lagging index's)
//! - Oracle index: every `Apply` = the chain's block at its height (P1), the `Step` contract (P2),
//!   final blocks never retracted and always the chain's (P3); at quiescence applied = best path,
//!   durable = the path through the final tip (P4)

use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};

use super::{Answer, Input, Output, ProducerCore, HEDGE};
use crate::producer::checked::check_block;

const DEPTH: u32 = 3;
const SOURCES: usize = 4;
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

/// `Reorg` = top `depth` replaced by `len` heavier blocks (`len` < `depth` = a retreat)
#[derive(Debug, Clone)]
enum Change {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Finalize,
}

/// - `Chain.publish` = false: coalesced with the next publish (a `watch` keeps the latest)
/// - `Advance` = seconds, then a tick and every answer due
/// - `Restart` = fresh core from the index's durable tip (+ one `lagging` blocks behind); `reset` =
///   header store lost (best path re-verified, nothing final: P6 waits)
#[derive(Debug, Clone)]
enum Move {
    Chain { change: Change, publish: bool },
    Advance(u8),
    Restart { lagging: Option<u8>, reset: bool },
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let change = prop_oneof![
        3 => (1u32..=6).prop_map(Change::Extend),
        3 => (1u32..=6, 1u32..=6).prop_map(|(depth, len)| Change::Reorg { depth, len }),
        2 => Just(Change::Finalize),
    ];
    let one = prop_oneof![
        8 => (change, prop::bool::weighted(0.7))
            .prop_map(|(change, publish)| Move::Chain { change, publish }),
        4 => (0u8..=30).prop_map(Move::Advance),
        1 => (prop::option::of(0u8..=6), any::<bool>())
            .prop_map(|(lagging, reset)| Move::Restart { lagging, reset }),
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

    /// Every step's `check()` holds; the sink's steps replayed into a naive index follow the
    /// verified chain block for block, final data never retracts, and once the moves end the
    /// index holds exactly the best chain, durable through the final tip
    #[test]
    fn the_sink_follows_the_verified_best_chain_through_reorgs_lies_and_restarts(
        moves in moves(),
        kinds in kinds(),
        sources in 1usize..=SOURCES,
        lookahead in 1usize..=4,
        seed in any::<u64>(),
    ) {
        run(&moves, &kinds[..sources], lookahead, seed);
    }
}

/// The index the steps build (`Step` contract asserted on every step)
#[derive(Debug, Default)]
struct Index {
    durable: Vec<BlockHash>,
    applied: Vec<BlockHash>,
}

impl Index {
    fn tip(hashes: &[BlockHash]) -> Option<BlockRef> {
        let height = Height::try_from(u32::try_from(hashes.len()).ok()?.checked_sub(1)?).ok()?;
        Some(BlockRef { hash: *hashes.last()?, height })
    }

    /// P1–P3 per step: `chain` = what the core was last given
    fn take(&mut self, output: &Output, chain: &VerifiedChain, context: &str) {
        let final_len = chain.final_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let durable = self.durable.len();
        match output {
            Output::Apply { block, finalized } => {
                let (height, hash) = (block.header().height, block.header().hash);
                let at = u32::from(height) as usize;
                assert_eq!(chain.hash_at(height), Some(hash), "{context}: P1 Apply {height:?}");
                if *finalized && at < self.durable.len() {
                    assert_eq!(self.durable[at], hash, "{context}: replayed final {at} differs");
                    return;
                }
                assert_eq!(at, self.applied.len(), "{context}: P2 Apply {at} not contiguous");
                if *finalized {
                    let pending = self.applied.len() - self.durable.len();
                    assert_eq!(pending, 0, "{context}: P2 final Apply {at} over pending blocks");
                    self.durable.push(hash);
                }
                self.applied.push(hash);
            }
            Output::Finalized(height) => {
                let at = u32::from(*height) as usize;
                assert_eq!(at, self.durable.len(), "{context}: P2 Finalized {at} out of order");
                assert!(at < self.applied.len(), "{context}: P2 Finalized {at} never applied");
                self.durable.push(self.applied[at]);
            }
            Output::Reorg { .. } => self.applied.truncate(self.durable.len()),
            Output::Fetch { .. } | Output::Misanswered { .. } | Output::Unserved { .. } => {}
        }
        let grown = self.durable.len() > durable;
        assert!(!grown || self.durable.len() <= final_len, "{context}: P3 durable past final");
    }
}

struct Pending {
    due: Instant,
    source: usize,
    height: Height,
    hash: BlockHash,
    answer: Answer,
}

struct Sim<'a> {
    builder: Chain,
    headers: HeaderChain,
    decoy: BlockHash,
    kinds: &'a [Kind],
    lookahead: usize,
    core: ProducerCore,
    given: Option<Arc<VerifiedChain>>,
    pending: Vec<Pending>,
    now: Instant,
    rng: u64,
    index: Index,
}

impl Sim<'_> {
    fn random(&mut self, below: u64) -> u64 {
        // xorshift64*: deterministic from the case's seed
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) % below.max(1)
    }

    fn best_path(&self) -> Vec<BlockHash> {
        let chain = self.headers.verified().expect("genesis verified");
        let best = u32::from(chain.best().height);
        (0..=best).map(|h| chain.hash_at(height(h)).expect("best path")).collect()
    }

    fn change(&mut self, change: &Change) {
        let best = self.headers.best().expect("genesis verified").block;
        // a reset header store forgets finality: the index's durable tip still bounds a fork
        let durable = self.index.durable.len().saturating_sub(1) as u32;
        let floor = self.headers.final_tip().map_or(0, |tip| u32::from(tip.height)).max(durable);
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
    }

    fn publish(&mut self, context: &str) {
        let chain = Arc::new(self.headers.verified().expect("genesis verified"));
        self.given = Some(Arc::clone(&chain));
        for (at, hash) in self.index.durable.iter().enumerate() {
            let held = chain.hash_at(height(at as u32));
            assert_eq!(held, Some(*hash), "{context}: P3 final {at} off the verified chain");
        }
        self.step(Input::Chain(chain), context);
    }

    fn step(&mut self, input: Input, context: &str) {
        let outputs = self.core.step(input, self.now).expect("durable tips stay on the chain");
        self.core.check();
        let chain = Arc::clone(self.given.as_ref().expect("a chain before any output"));
        for output in outputs {
            self.index.take(&output, &chain, context);
            if let Output::Fetch { source, height, record } = output {
                self.ask(source, height, record);
            }
        }
    }

    /// The source's answer, through the real [`check_block`] (a lie never passes it: P5)
    fn ask(&mut self, source: usize, height: Height, record: zaino_header_chain::Record) {
        let kind = self.kinds[source];
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
        assert_eq!(matches!(answer, Answer::Checked(_)), honest, "P5: {kind:?} caught by check");
        let latency = match kind {
            Kind::Slow => HEDGE.as_secs() + 1 + self.random(5),
            _ => self.random(4),
        };
        let due = self.now + Duration::from_secs(latency);
        self.pending.push(Pending { due, source, height, hash: record.hash, answer });
    }

    /// Every answer due by now, in a random order
    fn answer(&mut self, context: &str) {
        loop {
            let due: Vec<usize> =
                (0..self.pending.len()).filter(|at| self.pending[*at].due <= self.now).collect();
            if due.is_empty() {
                return;
            }
            let pick = due[self.random(due.len() as u64) as usize];
            let Pending { source, height, hash, answer, .. } = self.pending.swap_remove(pick);
            self.step(Input::Answer { source, height, hash, answer }, context);
        }
    }

    fn advance(&mut self, seconds: u64, context: &str) {
        self.now += Duration::from_secs(seconds);
        if self.given.is_some() {
            self.step(Input::Tick, context);
        }
        self.answer(context);
    }

    fn restart(&mut self, lagging: Option<u8>, reset: bool, context: &str) {
        if reset {
            let genesis = self.builder.genesis().hash;
            let depth = ReorgDepth::new(std::num::NonZeroU32::new(DEPTH).expect("nz"));
            let tip = *self.best_path().last().expect("genesis");
            self.headers = HeaderChain::regtest_in_memory(genesis, depth);
            self.headers.insert_blocks(&self.builder.path(tip)).expect("the best path verifies");
        }
        let tip = Index::tip(&self.index.durable);
        let behind = |back: u8| {
            let len = self.index.durable.len().checked_sub(usize::from(back) + 1)?;
            Index::tip(&self.index.durable[..=len])
        };
        let durable = [tip, lagging.and_then(behind).or(tip)];
        self.core = ProducerCore::new(self.kinds.len(), self.lookahead, durable);
        self.pending.clear();
        self.index.applied = self.index.durable.clone();
        self.publish(context);
    }

    /// P4 + P5: the index converges on the best chain with every honest answer arriving
    fn settle(&mut self) {
        // final past every durable tip (a reset header store may sit below them)
        self.change(&Change::Extend(DEPTH));
        self.change(&Change::Finalize);
        self.publish("settle");
        let path = self.best_path();
        let chain = Arc::clone(self.given.as_ref().expect("published"));
        let final_len = chain.final_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        for second in 0..SETTLE {
            let durable_ok = self.index.durable.len() == final_len;
            if self.index.applied == path && durable_ok {
                assert_eq!(self.index.durable[..], path[..final_len], "P4: durable = final path");
                return;
            }
            self.advance(1, &format!("settle {second}s"));
        }
        let first_off = self.index.applied.iter().zip(&path).position(|(a, b)| a != b);
        panic!(
            "P4/P5: never settled: applied {} of {} (first off-best {first_off:?}), durable {} of {}",
            self.index.applied.len(),
            path.len(),
            self.index.durable.len(),
            final_len
        );
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small chain")
}

fn run(moves: &[Move], kinds: &[Kind], lookahead: usize, seed: u64) {
    let mut builder = Chain::new();
    let genesis = builder.genesis().hash;
    let decoy = builder.mine(genesis).hash;
    let depth = ReorgDepth::new(std::num::NonZeroU32::new(DEPTH).expect("nz"));
    let mut headers = HeaderChain::regtest_in_memory(genesis, depth);
    headers.insert_blocks(&builder.path(genesis)).expect("genesis");
    let mut sim = Sim {
        builder,
        headers,
        decoy,
        kinds,
        lookahead,
        core: ProducerCore::new(kinds.len(), lookahead, [None]),
        given: None,
        pending: Vec::new(),
        now: Instant::now(),
        rng: seed | 1,
        index: Index::default(),
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
            Move::Advance(seconds) => sim.advance(u64::from(*seconds), &context),
            Move::Restart { lagging, reset } => sim.restart(*lagging, *reset, &context),
        }
    }
    sim.settle();
}
