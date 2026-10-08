//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::VerifiedChain;
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::testing::Lie;
use zaino_sync::check_block;

use super::{Indexes, Input, NfsCore, Output};
use crate::fired;
use crate::graph::Node;

fn h(n: u32) -> Height {
    Height::try_from(n).expect("h")
}

fn both() -> Indexes {
    Indexes::one(0).union(Indexes::one(1))
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

    fn node(&self, hash: BlockHash) -> Node<Height> {
        let at = self.at(hash);
        let parent = self.builder.block(hash).header().prev_hash;
        Node { at, parent, block: self.block(hash), folded: Arc::new(at.height) }
    }

    /// Header chain over `blocks` (genesis first), finalized through `final_height`
    fn verified(&self, blocks: &[BlockHash], final_height: u32) -> Arc<VerifiedChain> {
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nz"));
        let mut headers = self.builder.header_chain(depth);
        let path: Vec<Arc<Block>> = blocks.iter().map(|hash| self.block(*hash)).collect();
        insert(&mut headers, &path).expect("valid");
        let through = self.at(blocks[final_height as usize]);
        headers.finalize(through);
        Arc::new(headers.verified().expect("verified"))
    }

    /// Both indexes durable at `durable`
    fn durable(&self, durable: [BlockHash; 2]) -> Input<Height> {
        Input::Durable(durable.iter().map(|hash| Some(self.at(*hash))).collect())
    }

    /// `input`, then every fetch and fold answered honestly (payload = height) unless held
    fn drive(&self, core: &mut NfsCore<Height>, input: Input<Height>, hold: Hold) {
        for output in core.step(input) {
            let input = match output {
                Output::Fetch { at, record } if Some(at.hash) != hold.fetch => {
                    let block = Block::clone(&self.block(at.hash));
                    Input::Body(check_block(block, at.height, &record).expect("honest"))
                }
                Output::Fold { at, covers, .. } if Some(at.hash) != hold.fold => {
                    Input::Folded { at, covers, folded: Arc::new(at.height) }
                }
                _ => continue,
            };
            self.drive(core, input, hold);
        }
    }

    /// - Window 10; durable A1 / A1; best S6, then A9 (final 4); durable A3 / A2 (root A2)
    /// - nodes A3..=A6 + side S5, S6 (fork A4 = final); A7 folding, A8 ready, A9 asked
    fn valid(&self) -> NfsCore<Height> {
        let a = &self.a;
        let all = Hold { fetch: None, fold: None };
        let mut core = NfsCore::new(4, 2, 10);
        self.drive(&mut core, Input::Chain(self.verified(&a[..=4], 1)), all);
        self.drive(&mut core, self.durable([a[1], a[1]]), all);
        let on_s = [&a[..=4], &self.s[..2]].concat();
        self.drive(&mut core, Input::Chain(self.verified(&on_s, 1)), all);
        let back_on_a = self.verified(&[&on_s[..], &a[5..]].concat(), 4);
        let hold = Hold { fetch: Some(a[9]), fold: Some(a[7]) };
        self.drive(&mut core, Input::Chain(back_on_a), hold);
        self.drive(&mut core, self.durable([a[3], a[2]]), all);
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
    let shown = valid.shown.as_ref().map(|shown| shown.tip);
    assert_eq!(
        (nodes, ready, folding, valid.wanted.get(&h(9)) == Some(&a[9]), shown),
        (expected, vec![h(8)], vec![a[7]], true, Some(world.at(a[6]))),
        "the planted state"
    );

    type Plant<'a> = Box<dyn Fn(&mut NfsCore<Height>) + 'a>;
    let drills: Vec<(&str, Plant)> = vec![
        ("nothing before a chain", Box::new(|c| c.chain = None)),
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
        ("N5: nothing folded outside the window", Box::new(|c| c.window = 2)),
        (
            "N4: served tip = the deepest folded best block",
            Box::new(|c| c.shown.as_mut().expect("published").tip = world.at(a[5])),
        ),
        (
            "fetch: ready bodies above the root",
            Box::new(|c| drop(c.ready.insert(h(2), world.block(a[2])))),
        ),
        (
            "N1: every ready body is best",
            Box::new(|c| drop(c.ready.insert(h(8), world.block(s[3])))),
        ),
        (
            "fetch: a folded block is not ready",
            Box::new(|c| drop(c.ready.insert(h(6), world.block(a[6])))),
        ),
        ("fetch: a ready body is not wanted", Box::new(|c| _ = c.wanted.insert(h(8), a[8]))),
        ("fetch: wants above the root", Box::new(|c| _ = c.wanted.insert(h(1), a[1]))),
        ("N1: every want is best", Box::new(|c| _ = c.wanted.insert(h(9), BlockHash::ZERO))),
        ("fetch: a folded block is not wanted", Box::new(|c| _ = c.wanted.insert(h(6), a[6]))),
        (
            "fold: nothing folds twice",
            Box::new(|c| drop(c.folding.insert(a[6], (world.block(a[6]), both())))),
        ),
    ];
    for (expected, plant) in drills {
        let mut core = world.valid();
        plant(&mut core);
        let message = fired(|| core.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: caller / driver bug → named panic
    let step = |input: Input<Height>| fired(|| drop(world.valid().step(input)));
    let preconditions = [
        ("at least one block in flight", fired(|| drop(NfsCore::<Height>::new(0, 1, 10)))),
        ("1 to 32 indexes", fired(|| drop(NfsCore::<Height>::new(1, 0, 10)))),
        ("one durable tip per enabled index", step(Input::Durable(vec![]))),
        ("N3: index 0's durable tip never moves back", step(world.durable([a[2], a[2]]))),
    ];
    for (expected, message) in preconditions {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    // stale fold result (no fold in flight, or another covers): ignored, no node
    let mut core = world.valid();
    for (at, covers) in [(a[8], both()), (a[7], Indexes::one(0))] {
        let stale = Input::Folded { at: world.at(at), covers, folded: Arc::new(h(0)) };
        assert!(core.step(stale).is_empty(), "{at:?}: nothing out");
        assert!(!core.graph.contains(&at), "{at:?}: never a node");
    }
    core.check();

    // root A2 past the window of best A9 (bulk sync): every node, body, fold and want dropped,
    // the root published (committed views alone)
    let mut core = world.valid();
    core.window = 6;
    let outputs = core.step(world.durable([a[3], a[2]]));
    let abandoned = outputs.iter().any(|out| matches!(out, Output::Abandon(at) if at.hash == a[9]));
    let root = outputs.iter().any(|out| {
        matches!(out, Output::Publish(Some(tip)) if tip.tip == world.at(a[2]) && tip.graph.is_empty())
    });
    assert!(abandoned && root, "A9 abandoned, root A2 published: {outputs:?}");
    let idle = core.graph.is_empty() && core.ready.is_empty() && core.folding.is_empty();
    assert!(idle && core.wanted.is_empty(), "nothing held outside the window");
    core.check();
}
