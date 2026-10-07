//! Fire drills (`verified-chain.md` §10 layer 4): each check in `check()` and each precondition
//! assert, seen firing on a planted bug (a check never seen firing is not known to work)

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};

use super::{Answer, Input, Output, ProducerCore, Want};
use crate::producer::checked::{check_block, Checked};

/// Panic message of `run`, `None` = it returned
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = catch_unwind(AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}

fn h(n: u32) -> Height {
    Height::try_from(n).expect("h")
}

/// Want for `hash`, never asked (two sources)
fn unasked(hash: BlockHash) -> Want {
    Want { hash, asked: Vec::new(), tried: vec![false; 2], retry_at: None }
}

/// Trunk A 0..=8 + side S 4..=8 off A3; `verified(tip, at)` = the header chain (depth 3) over A
/// through `tip`, finalized at its boundary once it held A through `at`
struct World {
    builder: Chain,
    a: Vec<BlockHash>,
    side: Vec<Block>,
}

impl World {
    fn new() -> Self {
        let mut builder = Chain::new();
        let a8 = builder.extend(builder.genesis().hash, 8);
        let a = builder.path(a8.hash).iter().map(|b| b.header().hash).collect::<Vec<_>>();
        let s8 = builder.extend(a[3], 5);
        let side = builder.path(s8.hash);
        Self { builder, a, side }
    }

    fn verified(&self, tip: usize, at: usize) -> Arc<VerifiedChain> {
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"));
        let mut headers = HeaderChain::regtest_in_memory(self.a[0], depth);
        let path = self.builder.path(self.a[tip]);
        headers.insert_blocks(&path[..=at]).expect("valid");
        if let Some(boundary) = headers.finalizable() {
            headers.finalize(boundary).expect("in-memory store");
        }
        headers.insert_blocks(&path[at + 1..]).expect("valid");
        Arc::new(headers.verified().expect("verified"))
    }

    /// Side block at `height`, checked against the side chain's own header
    fn side_checked(&self, height: u32) -> Checked {
        let side = VerifiedChain::regtest(&self.side);
        let record = side.header_at(h(height)).expect("on the side chain");
        check_block(self.side[height as usize].clone(), h(height), &record).expect("its own body")
    }

    /// Every fetch answered by its own source, honestly, unless `hold` names its height
    fn answer(&self, core: &mut ProducerCore, outputs: Vec<Output>, hold: &[u32]) {
        for output in outputs {
            let Output::Fetch { source, height, record } = output else { continue };
            if hold.contains(&u32::from(height)) {
                continue;
            }
            let block = self.builder.block(record.hash).clone();
            let answer = Answer::Checked(check_block(block, height, &record).expect("honest"));
            let input = Input::Answer { source, height, hash: record.hash, answer };
            let more = core.step(input, Instant::now()).expect("no durable tip");
            self.answer(core, more, hold);
        }
    }

    /// Window A3..=A6 (final through 2), A8 ready, A7 asked of a source and unanswered
    fn valid(&self) -> ProducerCore {
        let mut core = ProducerCore::new(2, 4, [None]);
        let outputs = core.step(Input::Chain(self.verified(5, 5)), Instant::now());
        self.answer(&mut core, outputs.expect("no durable tip"), &[]);
        let outputs = core.step(Input::Chain(self.verified(8, 5)), Instant::now());
        self.answer(&mut core, outputs.expect("no durable tip"), &[7]);
        core
    }
}

#[test]
fn every_invariant_check_fires_on_its_planted_bug() {
    let world = World::new();
    let valid = world.valid();
    valid.check();
    let window: Vec<u32> = valid.window.iter().map(|b| u32::from(b.height())).collect();
    let ready: Vec<u32> = valid.ready.keys().map(|height| u32::from(*height)).collect();
    let wants: Vec<u32> = valid.wants.keys().map(|height| u32::from(*height)).collect();
    assert_eq!((window, ready, wants), (vec![3, 4, 5, 6], vec![8], vec![7]), "the planted state");

    type Plant = Box<dyn Fn(&mut ProducerCore)>;
    let (side4, side8) = (world.side_checked(4), world.side_checked(8));
    let (a5, a8) = (world.a[5], world.a[8]);
    let finalized = world.verified(8, 8);
    let drills: Vec<(&str, Plant)> = vec![
        ("P1: nothing before a chain", Box::new(|c| c.chain = None)),
        ("P3: one finality, the chain's", Box::new(|c| c.final_seen = None)),
        (
            "P1: every delivered non-final block is best",
            Box::new(move |c| c.window[1] = side4.clone()),
        ),
        (
            "P1: every ready block is best",
            Box::new(move |c| {
                c.ready.insert(h(8), side8.clone());
            }),
        ),
        (
            "P2: window contiguous from the last final height",
            Box::new(|c| {
                c.window.remove(1);
            }),
        ),
        (
            "P3: no final block left pending",
            Box::new(move |c| {
                c.final_seen = finalized.final_tip();
                c.chain = Some(Arc::clone(&finalized));
            }),
        ),
        (
            "P3: only final heights announced final",
            Box::new(|c| {
                c.window.clear();
                c.announced = Some(h(6));
            }),
        ),
        (
            "P6: nothing delivered before every durable tip checks",
            Box::new(move |c| c.unchecked.push(BlockRef { hash: a8, height: h(8) })),
        ),
        (
            "P2: ready blocks above the delivered tip",
            Box::new(|c| {
                let block = c.window[1].clone();
                c.ready.insert(h(4), block);
            }),
        ),
        (
            "P2: wants above the delivered tip",
            Box::new(move |c| {
                c.wants.insert(h(5), unasked(a5));
            }),
        ),
        (
            "P1: every want is best",
            Box::new(|c| c.wants.get_mut(&h(7)).expect("wanted").hash = BlockHash::from([7; 32])),
        ),
        (
            "fetch: a ready block is not wanted",
            Box::new(move |c| {
                c.wants.insert(h(8), unasked(a8));
            }),
        ),
        (
            "fetch: load counts every ask in flight",
            Box::new(|c| c.sources.iter_mut().for_each(|source| source.load = 0)),
        ),
    ];
    for (expected, plant) in drills {
        let mut core = world.valid();
        plant(&mut core);
        let message = fired(|| core.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: a caller (or driver) bug panics naming the invariant
    let now = Instant::now();
    let (a6, a7) = (world.a[6], world.a[7]);
    let a7_checked = {
        let record = world.verified(8, 5).header_at(h(7)).expect("best");
        check_block(world.builder.block(a7).clone(), h(7), &record).expect("honest")
    };
    let rewound = world.verified(4, 4);
    let same = world.verified(8, 5);
    type Call<'a> = Box<dyn Fn() -> Option<String> + 'a>;
    let preconditions: Vec<(&str, Call)> = vec![
        ("a source to fetch from", Box::new(|| fired(|| drop(ProducerCore::new(0, 1, [None]))))),
        (
            "at least one block in flight",
            Box::new(|| fired(|| drop(ProducerCore::new(1, 0, [None])))),
        ),
        (
            "a sink with a subscriber",
            Box::new(|| fired(|| drop(ProducerCore::new(1, 1, Vec::<Option<BlockRef>>::new())))),
        ),
        (
            "P3: the final tip never moves back",
            Box::new(|| {
                let mut core = world.valid();
                fired(|| drop(core.step(Input::Chain(Arc::clone(&rewound)), now)))
            }),
        ),
        (
            "P3: a final block never changes",
            Box::new(|| {
                let mut core = world.valid();
                core.final_seen = Some(BlockRef { hash: a6, height: h(2) });
                fired(|| drop(core.step(Input::Chain(Arc::clone(&same)), now)))
            }),
        ),
        (
            "an answer for an ask in flight",
            Box::new(|| {
                let mut core = world.valid();
                core.sources.iter_mut().for_each(|source| source.load = 0);
                let input =
                    Input::Answer { source: 0, height: h(7), hash: a7, answer: Answer::Failed };
                fired(|| drop(core.step(input, now)))
            }),
        ),
        (
            "P1: a checked block is the one asked for",
            Box::new(|| {
                let mut core = world.valid();
                let answer = Answer::Checked(a7_checked.clone());
                let input = Input::Answer { source: 0, height: h(7), hash: a6, answer };
                core.sources.iter_mut().for_each(|source| source.load = 1);
                fired(|| drop(core.step(input, now)))
            }),
        ),
        (
            "P1: ready blocks follow the chain",
            Box::new(|| {
                let mut core = world.valid();
                core.ready.insert(h(7), world.side_checked(8));
                fired(|| drop(core.step(Input::Tick, now)))
            }),
        ),
        (
            "P2: a final Apply only with nothing pending",
            Box::new(|| {
                let mut core = world.valid();
                core.final_seen = Some(BlockRef { hash: a7, height: h(7) });
                core.ready.insert(h(7), a7_checked.clone());
                fired(|| drop(core.step(Input::Tick, now)))
            }),
        ),
    ];
    for (expected, call) in preconditions {
        let message = call().unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }
}
