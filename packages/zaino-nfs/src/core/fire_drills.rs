//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::VerifiedChain;
use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::Lie;
use zaino_sync::check_block;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use super::{Indexes, Input, NfsCore, Output};
use crate::graph::Node;

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
        self.builder.block(hash).block_ref()
    }

    fn block(&self, hash: BlockHash) -> Arc<Block> {
        Arc::clone(self.builder.block(hash))
    }

    fn node(&self, hash: BlockHash) -> Node<BlockHeight> {
        let at = self.at(hash);
        let parent = self.builder.block(hash).header().prev_hash;
        Node { at, parent, block: self.block(hash), folded: Arc::new(at.height) }
    }

    /// Header chain over `blocks` (genesis first), finalized through `finalized_height`
    fn verified(&self, blocks: &[BlockHash], finalized_height: u32) -> Arc<VerifiedChain> {
        let depth = FinalityDepth::new(NonZeroU32::new(3).expect("nz"));
        let mut headers = self.builder.header_chain(depth);
        let path: Vec<Arc<Block>> = blocks.iter().map(|hash| self.block(*hash)).collect();
        insert(&mut headers, &path).expect("valid");
        let through = self.at(blocks[finalized_height as usize]);
        headers.finalize(through);
        Arc::new(headers.verified().expect("verified"))
    }

    /// Both indexes durable at `durable`
    fn durable(&self, durable: [BlockHash; 2]) -> Input<BlockHeight> {
        Input::Durable(durable.iter().map(|hash| Some(self.at(*hash))).collect())
    }

    /// `input`, then every fetch and fold answered honestly (payload = height) unless held
    fn drive(&self, core: &mut NfsCore<BlockHeight>, input: Input<BlockHeight>, hold: Hold) {
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

    /// - Window 10; durable A1 / A1; best S6, then A9 (finalized 4); durable A3 / A2 (root A2)
    /// - nodes A3..=A6 + side S5, S6 (fork A4 = finalized); A7 folding, A8 ready, A9 asked
    fn valid(&self) -> NfsCore<BlockHeight> {
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
fn the_unplanted_core_passes_every_check_in_its_planted_state() {
    let world = World::new();
    let (a, s) = (&world.a, &world.s);
    let valid = world.valid();
    valid.check();
    let nodes: HashSet<BlockHash> = valid.graph.nodes().map(|node| node.at.hash).collect();
    let expected = HashSet::from([a[3], a[4], a[5], a[6], s[0], s[1]]);
    let ready: Vec<BlockHeight> = valid.ready.keys().copied().collect();
    let folding: Vec<BlockHash> = valid.folding.keys().copied().collect();
    let shown = valid.shown.as_ref().map(|shown| shown.tip);
    assert_eq!(
        (nodes, ready, folding, valid.wanted.get(&h(9)) == Some(&a[9]), shown),
        (expected, vec![h(8)], vec![a[7]], true, Some(world.at(a[6]))),
    );
}

#[test]
#[should_panic(expected = "nothing before a chain")]
fn check_catches_a_core_without_a_chain() {
    let world = World::new();
    let mut core = world.valid();
    core.chain = None;
    core.check();
}

#[test]
#[should_panic(expected = "N1: every node's block = its own header")]
fn check_catches_a_node_holding_another_block() {
    let world = World::new();
    let mut core = world.valid();
    core.graph.insert(Node { block: world.block(world.a[6]), ..world.node(world.a[5]) });
    core.check();
}

#[test]
#[should_panic(expected = "N1: every node's body = its header's merkle root")]
fn check_catches_a_node_body_off_its_merkle_root() {
    let world = World::new();
    let mut core = world.valid();
    let block = Arc::new(Lie::Poisoned.told(&world.block(world.a[5])));
    core.graph.insert(Node { block, ..world.node(world.a[5]) });
    core.check();
}

#[test]
#[should_panic(expected = "N2: nodes only above the root")]
fn check_catches_a_node_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.valid();
    core.graph.insert(world.node(world.a[2]));
    core.check();
}

#[test]
#[should_panic(expected = "N2: every node folds on a held parent")]
fn check_catches_a_node_whose_parent_is_gone() {
    let world = World::new();
    let mut core = world.valid();
    core.graph.remove(&world.a[5]);
    core.check();
}

#[test]
#[should_panic(expected = "G8: every node on the best chain or a side branch it holds")]
fn check_catches_a_node_on_a_branch_the_chain_does_not_hold() {
    let world = World::new();
    let mut core = world.valid();
    core.graph.insert(world.node(world.q[0]));
    core.check();
}

#[test]
#[should_panic(expected = "G8: every node on the best chain or a side branch it holds")]
fn check_catches_a_side_node_past_what_the_chain_holds() {
    let world = World::new();
    let mut core = world.valid();
    core.graph.insert(world.node(world.s[2]));
    core.check();
}

#[test]
#[should_panic(expected = "N5: nothing folded outside the window")]
fn check_catches_a_node_outside_the_window() {
    let world = World::new();
    let mut core = world.valid();
    core.window = 2;
    core.check();
}

#[test]
#[should_panic(expected = "N4: served tip = the deepest folded best block")]
fn check_catches_a_served_tip_short_of_the_deepest_fold() {
    let world = World::new();
    let mut core = world.valid();
    core.shown.as_mut().expect("published").tip = world.at(world.a[5]);
    core.check();
}

#[test]
#[should_panic(expected = "fetch: ready bodies above the root")]
fn check_catches_a_ready_body_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.valid();
    core.ready.insert(h(2), world.block(world.a[2]));
    core.check();
}

#[test]
#[should_panic(expected = "N1: every ready body is best")]
fn check_catches_a_ready_body_off_the_best_chain() {
    let world = World::new();
    let mut core = world.valid();
    core.ready.insert(h(8), world.block(world.s[3]));
    core.check();
}

#[test]
#[should_panic(expected = "fetch: a folded block is not ready")]
fn check_catches_a_folded_block_still_ready() {
    let world = World::new();
    let mut core = world.valid();
    core.ready.insert(h(6), world.block(world.a[6]));
    core.check();
}

#[test]
#[should_panic(expected = "fetch: a ready body is not wanted")]
fn check_catches_a_ready_body_still_wanted() {
    let world = World::new();
    let mut core = world.valid();
    core.wanted.insert(h(8), world.a[8]);
    core.check();
}

#[test]
#[should_panic(expected = "fetch: wants above the root")]
fn check_catches_a_want_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.valid();
    core.wanted.insert(h(1), world.a[1]);
    core.check();
}

#[test]
#[should_panic(expected = "N1: every want is best")]
fn check_catches_a_want_off_the_best_chain() {
    let world = World::new();
    let mut core = world.valid();
    core.wanted.insert(h(9), BlockHash([0; 32]));
    core.check();
}

#[test]
#[should_panic(expected = "fetch: a folded block is not wanted")]
fn check_catches_a_folded_block_still_wanted() {
    let world = World::new();
    let mut core = world.valid();
    core.wanted.insert(h(6), world.a[6]);
    core.check();
}

#[test]
#[should_panic(expected = "fold: nothing folds twice")]
fn check_catches_a_folded_block_folding_again() {
    let world = World::new();
    let mut core = world.valid();
    core.folding.insert(world.a[6], (world.block(world.a[6]), both()));
    core.check();
}

#[test]
#[should_panic(expected = "at least one block in flight")]
fn a_core_with_no_block_in_flight_panics() {
    NfsCore::<BlockHeight>::new(0, 1, 10);
}

#[test]
#[should_panic(expected = "1 to 32 indexes")]
fn a_core_with_no_index_panics() {
    NfsCore::<BlockHeight>::new(1, 0, 10);
}

#[test]
#[should_panic(expected = "one durable tip per enabled index")]
fn durable_tips_short_of_the_indexes_panic() {
    World::new().valid().step(Input::Durable(vec![]));
}

#[test]
#[should_panic(expected = "N3: index 0's durable tip never moves back")]
fn a_durable_tip_moving_back_panics() {
    let world = World::new();
    world.valid().step(world.durable([world.a[2], world.a[2]]));
}

/// No fold in flight, or another covers: ignored, never a node
#[test]
fn a_stale_fold_result_is_ignored() {
    let world = World::new();
    let a = &world.a;
    let mut core = world.valid();
    for (at, covers) in [(a[8], both()), (a[7], Indexes::one(0))] {
        let stale = Input::Folded { at: world.at(at), covers, folded: Arc::new(h(0)) };
        assert!(core.step(stale).is_empty(), "{at:?}: nothing out");
        assert!(!core.graph.contains(&at), "{at:?}: never a node");
    }
    core.check();
}

/// Root A2 past the window of best A9 (bulk sync): every node, body, fold and want dropped, the
/// root published (durable views alone)
#[test]
fn a_root_past_the_window_drops_everything_and_publishes_the_root() {
    let world = World::new();
    let a = &world.a;
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
