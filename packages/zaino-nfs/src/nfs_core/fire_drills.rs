//! Fire drills: each `check()` assertion and precondition seen
//! firing on a planted bug (never seen firing = not known to work)

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_header_chain::testing::insert;
use zaino_header_chain::{HeaderChain, HeaderChainSnapshot};
use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{h, Lie, MockChain};
use zaino_sync::testing::check_block;
use zaino_sync::IndexKind;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use super::{DurableTips, Input, NfsCore, Output};
use crate::graph::Node;
use crate::per_index::IndexSet;

/// Two indexes, apart in `IndexKind::ALL`
const BOTH: [IndexKind; 2] = [IndexKind::CompactBlock, IndexKind::TxidLocation];

fn both() -> IndexSet {
    BOTH.into_iter().collect()
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

    /// `A3`, `S5`, `Q4` (`?` = a block off every branch)
    fn name(&self, hash: BlockHash) -> String {
        let branches = [("A", &self.a, 0), ("S", &self.s, 5), ("Q", &self.q, 4)];
        let named = branches.iter().find_map(|(branch, hashes, first)| {
            let at = hashes.iter().position(|each| *each == hash)?;
            Some(format!("{branch}{}", first + at))
        });
        named.unwrap_or_else(|| "?".to_owned())
    }

    /// Each output by block name (`Publish A2 (0 nodes)`)
    fn describe(&self, outputs: &[Output<BlockHeight>]) -> Vec<String> {
        let describe = |output: &Output<BlockHeight>| match output {
            Output::Fetch(at) => format!("Fetch {}", self.name(at.hash)),
            Output::Abandon(at) => format!("Abandon {}", self.name(at.hash)),
            Output::Fold { at, .. } => format!("Fold {}", self.name(at.hash)),
            Output::Publish(None) => "Publish nothing".to_owned(),
            Output::Publish(Some(tip)) => {
                let nodes = tip.graph.nodes().count();
                format!("Publish {} ({nodes} nodes)", self.name(tip.tip.hash))
            }
        };
        outputs.iter().map(describe).collect()
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
    fn snapshot(&self, blocks: &[BlockHash], finalized_height: usize) -> Arc<HeaderChainSnapshot> {
        let depth = FinalityDepth::new(NonZeroU32::new(3).expect("nz"));
        let mut headers = HeaderChain::anchored(&self.builder, depth);
        let path: Vec<Arc<Block>> = blocks.iter().map(|hash| self.block(*hash)).collect();
        insert(&mut headers, &path).expect("valid");
        let through = self.at(blocks[finalized_height]);
        headers.finalize(through);
        Arc::new(headers.snapshot().expect("verified"))
    }

    /// `BOTH` durable at `durable`
    fn durable(&self, durable: [BlockHash; 2]) -> Input<BlockHeight> {
        let tips = BOTH.into_iter().zip(durable).map(|(kind, hash)| (kind, Some(self.at(hash))));
        Input::Durable(tips.collect())
    }

    /// `input`, then every fetch and fold answered honestly (payload = height) unless held
    fn drive(&self, core: &mut NfsCore<BlockHeight>, input: Input<BlockHeight>, hold: Hold) {
        for output in core.step(input) {
            let input = match output {
                Output::Fetch(at) if Some(at.hash) != hold.fetch => {
                    let block = Block::clone(&self.block(at.hash));
                    Input::Body(check_block(block, at).expect("honest"))
                }
                Output::Fold { at, covers, .. } if Some(at.hash) != hold.fold => {
                    Input::Folded { at, covers, folded: Arc::new(at.height) }
                }
                _ => continue,
            };
            self.drive(core, input, hold);
        }
    }

    /// Core serving A6 over root A2, window 10, lookahead 4, `BOTH`
    ///
    /// - nodes A3..=A6 + side S5, S6 (best S6 first, then A9 finalized through A4)
    /// - A7 folding, A8 ready, A9 fetching; durable A3 / A2
    fn serving_a6(&self) -> NfsCore<BlockHeight> {
        let a = &self.a;
        let all = Hold { fetch: None, fold: None };
        let mut core = NfsCore::new(4, both(), 10);
        self.drive(&mut core, Input::Chain(self.snapshot(&a[..=4], 1)), all);
        self.drive(&mut core, self.durable([a[1], a[1]]), all);
        let on_s = [&a[..=4], &self.s[..2]].concat();
        self.drive(&mut core, Input::Chain(self.snapshot(&on_s, 1)), all);
        let back_on_a = self.snapshot(&[&on_s[..], &a[5..]].concat(), 4);
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

/// Core state by block name
#[derive(Debug, PartialEq, Eq)]
struct Held {
    nodes: BTreeSet<String>,
    ready: Vec<String>,
    folding: Vec<String>,
    wanted: Vec<String>,
    served: Option<String>,
}

#[test]
fn the_unplanted_core_serving_a6_passes_check() {
    let world = World::new();
    let core = world.serving_a6();
    core.check();

    let name = |hash: &BlockHash| world.name(*hash);
    let held = Held {
        nodes: core.graph.nodes().map(|node| name(&node.at.hash)).collect(),
        ready: core.ready.values().map(|block| name(&block.header().hash)).collect(),
        folding: core.folding.keys().map(name).collect(),
        wanted: core.wanted.values().map(name).collect(),
        served: core.shown.as_ref().map(|shown| name(&shown.tip.hash)),
    };
    let expected = Held {
        nodes: ["A3", "A4", "A5", "A6", "S5", "S6"].map(str::to_owned).into(),
        ready: vec!["A8".to_owned()],
        folding: vec!["A7".to_owned()],
        wanted: vec!["A9".to_owned()],
        served: Some("A6".to_owned()),
    };
    assert_eq!(held, expected, "serving_a6() builds what its doc names");
}

#[test]
#[should_panic(expected = "nothing before a chain")]
fn check_catches_a_core_without_a_chain() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.chain = None;
    core.check();
}

#[test]
#[should_panic(expected = "node at BlockHeight(5): every node's block = its own header (N1)")]
fn check_catches_a_node_holding_another_block() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.graph.insert(Node { block: world.block(world.a[6]), ..world.node(world.a[5]) });
    core.check();
}

#[test]
#[should_panic(expected = "node at BlockHeight(5): every node's body = its header's merkle root")]
fn check_catches_a_node_body_off_its_merkle_root() {
    let world = World::new();
    let mut core = world.serving_a6();
    let poisoned = Lie::Poisoned.told(world.builder.block_bytes(world.a[5]));
    let block = Arc::new(zaino_source::decode_block(&poisoned).expect("a lie still decodes"));
    core.graph.insert(Node { block, ..world.node(world.a[5]) });
    core.check();
}

#[test]
#[should_panic(expected = "node at BlockHeight(2), root Some(BlockHeight(2)): nodes only above")]
fn check_catches_a_node_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.graph.insert(world.node(world.a[2]));
    core.check();
}

#[test]
#[should_panic(expected = "every node folds on a held parent (N2)")]
fn check_catches_a_node_whose_parent_is_gone() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.graph.remove(&world.a[5]);
    core.check();
}

#[test]
#[should_panic(expected = "N8: every node on the best chain or a side branch it holds")]
fn check_catches_a_node_on_a_branch_the_chain_does_not_hold() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.graph.insert(world.node(world.q[0]));
    core.check();
}

/// Root A2, best A9: 7 blocks past the root, outside window 2
#[test]
#[should_panic(expected = "nothing folded outside the window (N5): root Some(BlockHeight(2)), \
                           best BlockHeight(9), window 2")]
fn check_catches_a_node_outside_the_window() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.window = 2;
    core.check();
}

#[test]
#[should_panic(expected = "published tip (left) = the deepest folded best block")]
fn check_catches_a_served_tip_short_of_the_deepest_fold() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.shown.as_mut().expect("published").tip = world.at(world.a[5]);
    core.check();
}

#[test]
#[should_panic(expected = "ready body at BlockHeight(2) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): ready bodies above the root only")]
fn check_catches_a_ready_body_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.ready.insert(h(2), world.block(world.a[2]));
    core.check();
}

#[test]
#[should_panic(expected = "ready body at BlockHeight(8) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): every ready body is best (N1)")]
fn check_catches_a_ready_body_off_the_best_chain() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.ready.insert(h(8), world.block(world.s[3]));
    core.check();
}

#[test]
#[should_panic(expected = "ready body at BlockHeight(6) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): a folded block is not ready")]
fn check_catches_a_folded_block_still_ready() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.ready.insert(h(6), world.block(world.a[6]));
    core.check();
}

#[test]
#[should_panic(expected = "ready body at BlockHeight(8) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): a ready body is not wanted")]
fn check_catches_a_ready_body_still_wanted() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.wanted.insert(h(8), world.a[8]);
    core.check();
}

#[test]
#[should_panic(expected = "want at BlockHeight(1) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): wants above the root only")]
fn check_catches_a_want_at_or_below_the_root() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.wanted.insert(h(1), world.a[1]);
    core.check();
}

#[test]
#[should_panic(expected = "want at BlockHeight(9) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): every want is best (N1)")]
fn check_catches_a_want_off_the_best_chain() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.wanted.insert(h(9), BlockHash([0; 32]));
    core.check();
}

#[test]
#[should_panic(expected = "want at BlockHeight(6) (root Some(BlockHeight(2)), best \
                           BlockHeight(9)): a folded block is not wanted")]
fn check_catches_a_folded_block_still_wanted() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.wanted.insert(h(6), world.a[6]);
    core.check();
}

#[test]
#[should_panic(expected = "folding at BlockHeight(6), already a node: nothing folds twice")]
fn check_catches_a_folded_block_folding_again() {
    let world = World::new();
    let mut core = world.serving_a6();
    core.folding.insert(world.a[6], (world.block(world.a[6]), both()));
    core.check();
}

#[test]
#[should_panic(expected = "at least one block in flight")]
fn a_core_with_no_block_in_flight_panics() {
    NfsCore::<BlockHeight>::new(0, both(), 10);
}

#[test]
#[should_panic(expected = "at least one index")]
fn a_core_with_no_index_panics() {
    NfsCore::<BlockHeight>::new(1, IndexSet::default(), 10);
}

#[test]
#[should_panic(expected = "one durable tip per enabled index")]
fn durable_tips_for_other_indexes_panic() {
    let others = [(IndexKind::TxidLocation, None), (IndexKind::UtxoSet, None)];
    let tips: DurableTips = others.into_iter().collect();
    World::new().serving_a6().step(Input::Durable(tips));
}

#[test]
#[should_panic(expected = "N3: compact_block's durable tip never moves back")]
fn a_durable_tip_moving_back_panics() {
    let world = World::new();
    world.serving_a6().step(world.durable([world.a[2], world.a[2]]));
}

#[test]
fn a_fold_result_no_fold_in_flight_asked_for_is_ignored() {
    let world = World::new();
    let a = &world.a;
    #[rustfmt::skip]
    let stale = [
        ("A8: nothing folding there",                 a[8], both()),
        ("A7: folding over both, result covers one",  a[7], IndexSet::default().with(BOTH[0])),
    ];
    for (case, at, covers) in stale {
        let mut core = world.serving_a6();

        let result = Input::Folded { at: world.at(at), covers, folded: Arc::new(h(0)) };
        let outputs = core.step(result);

        assert_eq!(
            world.describe(&outputs),
            Vec::<String>::new(),
            "{case}: outputs, none expected"
        );
        let node = core.graph.contains(&at);
        assert!(!node, "{case}: {} never a node", world.name(at));
        core.check();
    }
}

/// Root A2, best A9 = 7 blocks past it: folds iff window >= 7 (else bulk sync: the finalized path
/// alone, durable views at the root)
#[test]
fn the_core_folds_only_while_best_is_within_the_window_of_the_root() {
    struct Case {
        window: u32,
        outputs: &'static [&'static str],
        nodes: &'static [&'static str],
        ready_folding_wanted: (usize, usize, usize),
    }
    let world = World::new();
    let a = &world.a;
    let cases = [
        Case {
            window: 7,
            outputs: &[],
            nodes: &["A3", "A4", "A5", "A6", "S5", "S6"],
            ready_folding_wanted: (1, 1, 1),
        },
        Case {
            window: 6,
            outputs: &["Abandon A9", "Publish A2 (0 nodes)"],
            nodes: &[],
            ready_folding_wanted: (0, 0, 0),
        },
    ];
    for Case { window, outputs, nodes, ready_folding_wanted } in cases {
        let mut core = world.serving_a6();
        core.window = window;

        let stepped = core.step(world.durable([a[3], a[2]]));

        let case = format!("window {window}, best 7 blocks past the root");
        assert_eq!(world.describe(&stepped), outputs, "{case}: outputs (left) vs expected");
        let mut held: Vec<String> =
            core.graph.nodes().map(|node| world.name(node.at.hash)).collect();
        held.sort();
        assert_eq!(held, nodes, "{case}: nodes kept (left) vs expected");
        let held = (core.ready.len(), core.folding.len(), core.wanted.len());
        assert_eq!(held, ready_folding_wanted, "{case}: (ready, folding, wanted) vs expected");
        core.check();
    }
}
