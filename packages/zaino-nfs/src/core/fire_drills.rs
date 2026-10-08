//! Fire drills (`verified-chain.md` §10 layer 4): each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::VerifiedChain;
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::testing::Lie;

use super::{Diverged, Indexes, Input, NfsCore, Output, Sent};
use crate::fetch::{check_block, Checked};
use crate::fired;
use crate::graph::{Graph, Node};

fn h(n: u32) -> Height {
    Height::try_from(n).expect("h")
}

/// Each index its own group
fn alone(count: usize) -> Vec<Indexes> {
    (0..count).map(Indexes::one).collect()
}

fn depth() -> ReorgDepth {
    ReorgDepth::new(NonZeroU32::new(3).expect("nz"))
}

/// Trunk A 0..=9, side S 5..=8 off A4, side Q 4..=7 off A3
struct World {
    builder: MockChain,
    a: Vec<BlockHash>,
    s: Vec<BlockHash>,
    q: Vec<BlockHash>,
}

impl World {
    fn new() -> Self {
        let mut builder = MockChain::regtest();
        let hashes = |builder: &MockChain, tip: BlockRef| -> Vec<BlockHash> {
            builder.blocks(tip).iter().map(|block| block.header().hash).collect()
        };
        let a9 = builder.mine_empty(9);
        let a = hashes(&builder, a9);
        let s8 = builder.fork(h(4)).mine_empty(4).tip();
        let q7 = builder.fork(h(3)).mine_empty(4).tip();
        let (s, q) = (hashes(&builder, s8)[5..].to_vec(), hashes(&builder, q7)[4..].to_vec());
        Self { builder, a, s, q }
    }

    fn at(&self, hash: BlockHash) -> BlockRef {
        self.builder.block(hash).at()
    }

    fn block(&self, hash: BlockHash) -> Arc<Block> {
        Arc::clone(self.builder.block(hash))
    }

    /// Body of `hash` checked against its own branch's header
    fn checked(&self, hash: BlockHash) -> Checked {
        let at = self.at(hash);
        let record = self.builder.verified(at).header_at(at.height).expect("on its own branch");
        check_block(Block::clone(&self.block(hash)), at.height, &record).expect("its own body")
    }

    fn node(&self, hash: BlockHash) -> Node<Height> {
        let at = self.at(hash);
        let parent = self.builder.block(hash).header().prev_hash;
        let (block, folded, covers) = (self.block(hash), Arc::new(at.height), Indexes::first(2));
        Node { at, parent, block, folded, covers }
    }

    /// Header chain over `blocks` (genesis first), finalized through `final_height`
    fn verified(&self, blocks: &[BlockHash], final_height: u32) -> Arc<VerifiedChain> {
        let mut headers = self.builder.header_chain(depth());
        let path: Vec<Arc<Block>> = blocks.iter().map(|hash| self.block(*hash)).collect();
        insert(&mut headers, &path).expect("valid");
        let through = self.at(blocks[final_height as usize]);
        headers.finalize(through).expect("in-memory store");
        Arc::new(headers.verified().expect("verified"))
    }

    /// `input`, then every fetch and fold answered honestly (payload = height) unless held, every
    /// send delivered
    fn drive(&self, core: &mut NfsCore<Height>, input: Input<Height>, hold: Hold) {
        let outputs = core.step(input).expect("durable tips on the chain");
        for output in outputs {
            let input = match output {
                Output::Fetch { at, record, .. } if Some(at.hash) != hold.fetch => {
                    let block = Block::clone(&self.block(at.hash));
                    Input::Body(check_block(block, at.height, &record).expect("honest"))
                }
                Output::Fold { at, .. } if Some(at.hash) != hold.fold => {
                    Input::Folded { at, folded: Arc::new(at.height) }
                }
                Output::Send(_) => Input::Delivered,
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
        let mut core = NfsCore::new(4, vec![None, None], alone(2));
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
        (
            nodes,
            ready,
            folding,
            valid.wanted.get(&h(9)) == Some(&a[9]),
            valid.sent,
            valid.shown.tip
        ),
        (expected, vec![h(8)], vec![a[7]], true, sent, Some(world.at(a[6]))),
        "the planted state"
    );

    type Plant<'a> = Box<dyn Fn(&mut NfsCore<Height>) + 'a>;
    let drills: Vec<(&str, Plant)> = vec![
        ("nothing before a chain", Box::new(|c| c.chain = None)),
        ("J1: joined = whole groups, never none", Box::new(|c| c.joined = Indexes::default())),
        (
            "J1: joined = whole groups, never none",
            Box::new(|c| {
                c.groups = vec![Indexes::first(2)];
                c.joined = Indexes::one(0);
            }),
        ),
        (
            "J2: a lagging group at the root joins",
            Box::new(|c| {
                c.joined = Indexes::one(0);
                c.durable[1] = c.durable[0];
            }),
        ),
        ("N5: at most lookahead sends in flight", Box::new(|c| c.undelivered = 5)),
        (
            "N3: the stream at or past the lowest durable tip",
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
                let block = Arc::new(Lie::Poisoned.told(&world.block(a[5])));
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
            "J3: a node folds joined indexes only",
            Box::new(|c| {
                let covers = Indexes::first(2).union(Indexes::one(5));
                c.graph.insert(Node { covers, ..world.node(a[5]) });
            }),
        ),
        (
            "J4: a node's indexes ⊆ its parent node's",
            Box::new(|c| c.graph.insert(Node { covers: Indexes::one(0), ..world.node(a[5]) })),
        ),
        (
            "N3: a node sent final stays until every joined index holds it",
            Box::new(|c| [a[4], a[5], a[6], s[0], s[1]].iter().for_each(|h| c.graph.remove(h))),
        ),
        (
            "N4: served tip = the deepest folded best block",
            Box::new(|c| c.shown.tip = Some(world.at(a[5]))),
        ),
        ("N4: nothing served while restarting", Box::new(|c| c.durable[0] = Some(world.at(a[5])))),
        (
            "N4: published indexes = the served block's, the joined",
            Box::new(|c| c.shown.covers = Indexes::one(0)),
        ),
        (
            "N4: published indexes = the served block's, the joined",
            Box::new(|c| c.shown.joined = Indexes::one(0)),
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
        ("fetch: a ready body is not wanted", Box::new(|c| _ = c.wanted.insert(h(8), a[8]))),
        ("fetch: wants above the last sent", Box::new(|c| _ = c.wanted.insert(h(1), a[1]))),
        ("N1: every want is best", Box::new(|c| _ = c.wanted.insert(h(9), BlockHash::ZERO))),
        ("fetch: a folded block is not wanted", Box::new(|c| _ = c.wanted.insert(h(6), a[6]))),
        (
            "fold: nothing folds twice",
            Box::new(|c| drop(c.folding.insert(a[6], (world.block(a[6]), Indexes::first(2))))),
        ),
        (
            "J3: a fold covers joined indexes only",
            Box::new(|c| drop(c.folding.insert(a[8], (world.block(a[8]), Indexes::one(5))))),
        ),
    ];
    for (expected, plant) in drills {
        let mut core = world.valid();
        plant(&mut core);
        let message = fired(|| core.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: caller / driver bug → named panic
    let rewound = world.verified(&a[..=4], 1);
    let on_q = world.verified(&[&a[..=3], &q[..]].concat(), 4);
    let step = |plant: &dyn Fn(&mut NfsCore<Height>), input: Input<Height>| {
        let mut core = world.valid();
        plant(&mut core);
        fired(|| drop(core.step(input)))
    };
    let durable = |index, tip: BlockHash| Input::Durable { index, tip: Some(world.at(tip)) };
    let new = |durable: Vec<Option<BlockRef>>, groups| {
        fired(|| drop(NfsCore::<Height>::new(1, durable, groups)))
    };
    let at = |hash: BlockHash| Some(world.at(hash));
    let preconditions = [
        (
            "at least one block in flight",
            fired(|| drop(NfsCore::<Height>::new(0, vec![None], alone(1)))),
        ),
        ("an index to feed", new(vec![], vec![])),
        ("groups partition the indexes", new(vec![None, None], alone(1))),
        ("groups partition the indexes", new(vec![None, None], vec![Indexes::first(2); 2])),
        (
            "J2: an index joins at the root only",
            fired(|| {
                let mut core = world.valid();
                core.joined = Indexes::one(0);
                core.join(Indexes::one(1));
            }),
        ),
        (
            "N3: a fold on the root = every joined index durable at it",
            step(
                &|c| {
                    c.sent = Some(Sent { at: world.at(a[2]), folded: true });
                    c.durable = vec![at(a[2]), at(a[3])];
                    c.graph = Graph::new();
                    c.folding.clear();
                    c.ready.insert(h(3), world.checked(a[3]));
                },
                Input::Chain(world.verified(a, 4)),
            ),
        ),
        (
            "J4: a refold only widens a node's indexes",
            step(
                &|c| drop(c.folding.insert(a[6], (world.block(a[6]), Indexes::one(0)))),
                Input::Folded { at: world.at(a[6]), folded: Arc::new(h(6)) },
            ),
        ),
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
        ("a delivery for a send in flight", step(&|_| {}, Input::Delivered)),
        ("N3: a durable tip never moves back", step(&|_| {}, durable(0, a[2]))),
        ("N3: a durable tip is a block the stream sent", step(&|_| {}, durable(0, a[5]))),
    ];
    for (expected, message) in preconditions {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    // bad input (not a bug): durable tip off the final chain → `Err(Diverged)`, no panic
    let mut core = NfsCore::<Height>::new(1, vec![None, Some(world.at(q[0]))], alone(2));
    let diverged = core.step(Input::Chain(world.verified(a, 4))).map(drop);
    let expected = Diverged { index: 1, height: h(4), expected: q[0], got: a[4] };
    assert_eq!(diverged, Err(expected));
}
