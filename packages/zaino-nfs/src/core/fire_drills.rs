//! Fire drills (`verified-chain.md` §10 layer 4): each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};

use super::{Diverged, Input, NfsCore, Output, Sent};
use crate::fetch::{check_block, Answer, Checked};
use crate::fired;
use crate::graph::Node;

fn h(n: u32) -> Height {
    Height::try_from(n).expect("h")
}

fn depth() -> ReorgDepth {
    ReorgDepth::new(NonZeroU32::new(3).expect("nz"))
}

/// Trunk A 0..=9, side S 5..=8 off A4, side Q 4..=7 off A3
struct World {
    builder: Chain,
    a: Vec<BlockHash>,
    s: Vec<BlockHash>,
    q: Vec<BlockHash>,
}

impl World {
    fn new() -> Self {
        let mut builder = Chain::new();
        let hashes = |builder: &Chain, tip: BlockHash| -> Vec<BlockHash> {
            builder.path(tip).iter().map(|block| block.header().hash).collect()
        };
        let a9 = builder.extend(builder.genesis().hash, 9).hash;
        let a = hashes(&builder, a9);
        let s8 = builder.extend(a[4], 4).hash;
        let q7 = builder.extend(a[3], 4).hash;
        let (s, q) = (hashes(&builder, s8)[5..].to_vec(), hashes(&builder, q7)[4..].to_vec());
        Self { builder, a, s, q }
    }

    fn at(&self, hash: BlockHash) -> BlockRef {
        BlockRef { hash, height: self.builder.block(hash).header().height }
    }

    fn block(&self, hash: BlockHash) -> Arc<Block> {
        Arc::new(self.builder.block(hash).clone())
    }

    /// Body of `hash` checked against its own branch's header
    fn checked(&self, hash: BlockHash) -> Checked {
        let at = self.at(hash);
        let branch = VerifiedChain::regtest(&self.builder.path(hash));
        let record = branch.header_at(at.height).expect("on its own branch");
        check_block(self.builder.block(hash).clone(), at.height, &record).expect("its own body")
    }

    fn node(&self, hash: BlockHash) -> Node<Height> {
        let at = self.at(hash);
        let parent = self.builder.block(hash).header().prev_hash;
        Node { at, parent, block: self.block(hash), folded: Arc::new(at.height) }
    }

    /// Header chain over `blocks` (genesis first), finalized through `final_height`
    fn verified(&self, blocks: &[BlockHash], final_height: u32) -> Arc<VerifiedChain> {
        let mut headers = HeaderChain::regtest_in_memory(self.a[0], depth());
        let path: Vec<Block> =
            blocks.iter().map(|hash| self.builder.block(*hash).clone()).collect();
        headers.insert_blocks(&path).expect("valid");
        let through = self.at(blocks[final_height as usize]);
        headers.finalize(through).expect("in-memory store");
        Arc::new(headers.verified().expect("verified"))
    }

    /// `input`, then every fetch and fold answered honestly (payload = height) unless held
    fn drive(&self, core: &mut NfsCore<Height>, input: Input<Height>, hold: Hold) {
        let outputs = core.step(input, Instant::now()).expect("durable tips on the chain");
        for output in outputs {
            let input = match output {
                Output::Fetch { from, height, record } if Some(record.hash) != hold.fetch => {
                    let block = self.builder.block(record.hash).clone();
                    let answer = Answer::Checked(check_block(block, height, &record).expect("ok"));
                    Input::Body { from, at: BlockRef { hash: record.hash, height }, answer }
                }
                Output::Fold { at, .. } if Some(at.hash) != hold.fold => {
                    Input::Folded { at, folded: Arc::new(at.height) }
                }
                _ => continue,
            };
            self.drive(core, input, hold);
        }
    }

    /// - A0, A1 sent unfolded; A2..=A4 sent folded; durable A3 / A2 (root A2)
    /// - nodes A3..=A6 + side S5, S6 (fork A4 = final); A7 folding, A8 ready, A9 asked
    fn valid(&self) -> NfsCore<Height> {
        let a = &self.a;
        let all = Hold { fetch: None, fold: None };
        let mut core = NfsCore::new(2, 4, vec![None, None]);
        self.drive(&mut core, Input::Chain(self.verified(&a[..=4], 1)), all);
        for index in 0..2 {
            let tip = Some(self.at(a[1]));
            self.drive(&mut core, Input::Durable { index, tip }, all);
        }
        let on_s = [&a[..=4], &self.s[..2]].concat();
        self.drive(&mut core, Input::Chain(self.verified(&on_s, 1)), all);
        let back_on_a = self.verified(&[&on_s[..], &a[5..]].concat(), 4);
        let hold = Hold { fetch: Some(a[9]), fold: Some(a[7]) };
        self.drive(&mut core, Input::Chain(back_on_a), hold);
        self.drive(&mut core, Input::Durable { index: 0, tip: Some(self.at(a[3])) }, all);
        self.drive(&mut core, Input::Durable { index: 1, tip: Some(self.at(a[2])) }, all);
        core
    }
}

#[derive(Clone, Copy)]
struct Hold {
    fetch: Option<BlockHash>,
    fold: Option<BlockHash>,
}

#[test]
fn every_invariant_check_fires_on_its_planted_bug() {
    let world = World::new();
    let (a, s, q) = (&world.a, &world.s, &world.q);
    let valid = world.valid();
    valid.check();
    let nodes: BTreeSet<BlockHash> = valid.graph.nodes().map(|node| node.at.hash).collect();
    let expected = BTreeSet::from([a[3], a[4], a[5], a[6], s[0], s[1]]);
    let ready: Vec<Height> = valid.ready.keys().copied().collect();
    let folding: Vec<BlockHash> = valid.folding.keys().copied().collect();
    let sent = Some(Sent { at: world.at(a[4]), folded: true });
    assert_eq!(
        (nodes, ready, folding, valid.fetcher.wants(h(9)), valid.sent, valid.served),
        (expected, vec![h(8)], vec![a[7]], true, sent, Some(world.at(a[6]))),
        "the planted state"
    );

    type Plant<'a> = Box<dyn Fn(&mut NfsCore<Height>) + 'a>;
    let drills: Vec<(&str, Plant)> = vec![
        ("nothing before a chain", Box::new(|c| c.chain = None)),
        (
            "N3: root at or below the last block sent",
            Box::new(|c| c.durable.fill(Some(world.at(a[5])))),
        ),
        (
            "N5: nothing sent past the final tip",
            Box::new(|c| c.sent = Some(Sent { at: world.at(a[5]), folded: true })),
        ),
        (
            "N5: a sent block is never retracted",
            Box::new(|c| {
                let at = BlockRef { hash: a[3], height: h(4) };
                c.sent = Some(Sent { at, folded: true });
            }),
        ),
        (
            "N1: every node's block = its own header",
            Box::new(|c| c.graph.insert(Node { block: world.block(a[6]), ..world.node(a[5]) })),
        ),
        (
            "N1: every node's body = its header's merkle root",
            Box::new(|c| {
                let honest = world.block(a[5]);
                let extra = world.block(a[6]).transactions()[0].clone();
                let txs = [honest.transactions().to_vec(), vec![extra]].concat();
                let block = Arc::new(Block::new(honest.header().clone(), txs));
                c.graph.insert(Node { block, ..world.node(a[5]) });
            }),
        ),
        ("N2: nodes only above the root", Box::new(|c| c.graph.insert(world.node(a[2])))),
        ("N2: every node folds on a held parent", Box::new(|c| c.graph.remove(&a[5]))),
        (
            "G8: every node on the best chain or a side branch it holds",
            Box::new(|c| c.graph.insert(world.node(q[0]))),
        ),
        (
            "G8: every node on the best chain or a side branch it holds",
            Box::new(|c| c.graph.insert(world.node(s[2]))),
        ),
        (
            "N3: a node sent final stays until every index holds it durably",
            Box::new(|c| [a[4], a[5], a[6], s[0], s[1]].iter().for_each(|h| c.graph.remove(h))),
        ),
        (
            "N4: served tip = the deepest folded best block",
            Box::new(|c| c.served = Some(world.at(a[5]))),
        ),
        (
            "fetch: ready bodies above the last sent",
            Box::new(|c| drop(c.ready.insert(h(4), world.checked(a[4])))),
        ),
        (
            "N1: every ready body is best",
            Box::new(|c| drop(c.ready.insert(h(8), world.checked(s[3])))),
        ),
        (
            "fetch: a folded block is not ready",
            Box::new(|c| drop(c.ready.insert(h(6), world.checked(a[6])))),
        ),
        ("fetch: a ready body is not wanted", Box::new(|c| c.fetcher.want(world.at(a[8])))),
        (
            "fold: nothing folds twice",
            Box::new(|c| drop(c.folding.insert(a[6], world.checked(a[6])))),
        ),
    ];
    for (expected, plant) in drills {
        let mut core = world.valid();
        plant(&mut core);
        let message = fired(|| core.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: caller / driver bug → named panic
    let now = Instant::now();
    let rewound = world.verified(&a[..=4], 1);
    let on_q = world.verified(&[&a[..=3], &q[..]].concat(), 4);
    let step = |plant: &dyn Fn(&mut NfsCore<Height>), input: Input<Height>| {
        let mut core = world.valid();
        plant(&mut core);
        fired(|| drop(core.step(input, now)))
    };
    let durable = |index, tip: BlockHash| Input::Durable { index, tip: Some(world.at(tip)) };
    let preconditions = [
        ("at least one block in flight", fired(|| drop(NfsCore::<Height>::new(1, 0, vec![None])))),
        ("an index to feed", fired(|| drop(NfsCore::<Height>::new(1, 1, vec![])))),
        ("H2: the final tip never moves back", step(&|_| {}, Input::Chain(rewound))),
        (
            "H2: a final block never changes",
            step(&|c| c.chain = Some(Arc::clone(&on_q)), Input::Chain(world.verified(a, 4))),
        ),
        (
            "a fold result for a fold in flight",
            step(&|_| {}, Input::Folded { at: world.at(a[8]), folded: Arc::new(h(8)) }),
        ),
        ("a durable tip of an enabled index", step(&|_| {}, durable(2, a[4]))),
        ("N3: a durable tip never moves back", step(&|_| {}, durable(0, a[2]))),
        ("N3: a durable tip is a block the stream sent", step(&|_| {}, durable(0, a[5]))),
    ];
    for (expected, message) in preconditions {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    // bad input (not a bug): durable tip off the final chain → `Err(Diverged)`, no panic
    let mut core = NfsCore::<Height>::new(1, 1, vec![None, Some(world.at(q[0]))]);
    let diverged = core.step(Input::Chain(world.verified(a, 4)), now).map(drop);
    let expected = Diverged { index: 1, height: h(4), expected: q[0], got: a[4] };
    assert_eq!(diverged, Err(expected));
}
