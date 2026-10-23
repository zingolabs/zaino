//! Fire drills: each check in `check()` and each precondition assert, seen firing on a planted bug
//! (a check never seen firing is not known to work)

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_source::testing::{h, MockChain};
use zcash_protocol::BlockHash;

use super::{HeaderChain, Node};
use crate::testing::insert;
use crate::witnesses::Witnesses;
use crate::{decode_header, Inserted};

/// Trunk genesis ..= 12 + one side header, depth 9, finalized through 3 (`builder` mined both)
///
/// - `trunk[h]` = trunk at height `h`
/// - `side` = header at 12 on 11 (equal work, received after the trunk's 12)
struct TrunkAndSide {
    chain: HeaderChain,
    trunk: [BlockHash; 13],
    side: BlockHash,
    builder: MockChain,
}

fn trunk_and_side() -> TrunkAndSide {
    let mut builder = MockChain::regtest();
    let tip = builder.mine_empty(12);
    let trunk: Vec<BlockHash> = builder.blocks(tip).iter().map(|b| b.header().hash).collect();
    let side = builder.fork(h(11)).mine_empty(1).tip().hash;
    let depth = FinalityDepth::new(NonZeroU32::new(9).expect("nz"));
    let mut chain = HeaderChain::anchored(&builder, depth);
    insert(&mut chain, &builder.blocks(tip)).expect("valid");
    insert(&mut chain, &[Arc::clone(builder.block(side))]).expect("valid");
    chain.finalize(chain.finalizable().expect("12 − 9"));
    let trunk = trunk.try_into().expect("genesis ..= 12");
    TrunkAndSide { chain, trunk, side, builder }
}

/// Copy of side node `of` under a fresh hash: same parent and work, received last (bookkeeping
/// kept consistent, so only the bound breaks)
fn clone_side(chain: &mut HeaderChain, of: BlockHash, salt: u8) {
    let mut node: Node = chain.nodes[&of];
    let mut hash = of.0;
    hash[0] ^= salt;
    node.record.hash = BlockHash(hash);
    node.received = u64::MAX - u64::from(salt);
    chain.nodes.insert(node.record.hash, node);
    chain.leaves.insert(node.record.hash);
    if let Some(parent) = chain.nodes.get_mut(&node.parent) {
        parent.children += 1;
    }
}

#[test]
fn the_unplanted_chain_passes_every_check_and_knows_its_side_header() {
    let TrunkAndSide { mut chain, trunk, side, builder } = trunk_and_side();
    chain.check();
    let finalized = BlockRef { hash: trunk[3], height: h(3) };
    assert_eq!(chain.finalized_tip(), Some(finalized), "12 − depth 9");
    let best = chain.best().map(|best| best.block);
    assert_eq!(best, Some(BlockRef { hash: trunk[12], height: h(12) }), "equal work: first wins");
    let header = decode_header(&builder.header_bytes(side)).expect("real header");
    assert_eq!(chain.insert(&header), Ok(Inserted::Known), "side header held");
}

#[test]
#[should_panic(expected = "H1: best = the max-work leaf")]
fn check_catches_a_best_that_is_not_the_max_work_leaf() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    let side_record = chain.nodes[&side].record;
    chain.best_path.set(chain.best_path.len() - 1, side_record);
    chain.check();
}

#[test]
#[should_panic(expected = "does not descend from the finalized tip")]
fn check_catches_a_node_off_the_finalized_tip() {
    let TrunkAndSide { mut chain, trunk, .. } = trunk_and_side();
    chain.nodes.get_mut(&trunk[4]).expect("held").parent = BlockHash([9u8; 32]);
    chain.check();
}

#[test]
#[should_panic(expected = "H2: one height above its parent")]
fn check_catches_a_height_gap() {
    let TrunkAndSide { mut chain, trunk, .. } = trunk_and_side();
    chain.nodes.get_mut(&trunk[6]).expect("held").height = h(9);
    chain.check();
}

#[test]
#[should_panic(expected = "H2: nothing held before an anchor")]
fn check_catches_a_chain_without_an_anchor() {
    let TrunkAndSide { mut chain, .. } = trunk_and_side();
    chain.finalized_headers.clear();
    chain.check();
}

#[test]
#[should_panic(expected = "H2: finalized headers one height apart")]
fn check_catches_a_gap_in_the_finals() {
    let TrunkAndSide { mut chain, .. } = trunk_and_side();
    chain.finalized_headers.remove(1);
    chain.check();
}

#[test]
#[should_panic(expected = "H5: best_path record = its node")]
fn check_catches_a_best_path_record_off_its_node() {
    let TrunkAndSide { mut chain, .. } = trunk_and_side();
    let mut record = chain.best_path[2];
    record.time += 1;
    chain.best_path.set(2, record);
    chain.check();
}

#[test]
#[should_panic(expected = "H5: best_path at")]
fn check_catches_a_best_path_missing_a_height() {
    let TrunkAndSide { mut chain, .. } = trunk_and_side();
    chain.best_path.remove(2);
    chain.check();
}

#[test]
#[should_panic(expected = "H4: 37 side nodes, bound 36")]
fn check_catches_side_nodes_past_the_bound() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    // side + 36 clones = 37 > 4 · depth 9
    (1..=36).for_each(|salt| clone_side(&mut chain, side, salt));
    chain.check();
}

#[test]
#[should_panic(expected = "H4: 34 side tips, bound 32")]
fn check_catches_side_tips_past_the_bound() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    // side + 33 clones = 34 tips (34 nodes: under the node bound 36)
    (1..=33).for_each(|salt| clone_side(&mut chain, side, salt));
    chain.check();
}

#[test]
#[should_panic(expected = "tree: cumulative work")]
fn check_catches_cumulative_work_off_its_parent() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    chain.nodes.get_mut(&side).expect("held").record.cumulative_work += 1;
    chain.check();
}

#[test]
#[should_panic(expected = "tree: child count")]
fn check_catches_a_wrong_child_count() {
    let TrunkAndSide { mut chain, trunk, .. } = trunk_and_side();
    chain.nodes.get_mut(&trunk[11]).expect("held").children = 1;
    chain.check();
}

#[test]
#[should_panic(expected = "tree: leaf set")]
fn check_catches_a_leaf_missing_from_the_leaf_set() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    chain.leaves.remove(&side);
    chain.check();
}

#[test]
#[should_panic(expected = "witnessed, its parent not")]
fn check_catches_a_witnessed_node_under_an_unwitnessed_parent() {
    let TrunkAndSide { mut chain, trunk, .. } = trunk_and_side();
    chain.nodes.get_mut(&trunk[8]).expect("held").witnesses = Witnesses::default();
    chain.check();
}

#[test]
#[should_panic(expected = "H2: only a best-branch block becomes finalized")]
fn finalizing_a_block_off_the_best_branch_panics() {
    let TrunkAndSide { mut chain, side, .. } = trunk_and_side();
    chain.finalize(BlockRef { hash: side, height: h(12) });
}

#[test]
#[should_panic(expected = "H2: the finalized boundary stays `depth` below the best")]
fn finalizing_within_depth_of_the_best_panics() {
    let TrunkAndSide { mut chain, trunk, .. } = trunk_and_side();
    chain.finalize(BlockRef { hash: trunk[5], height: h(5) });
}

#[test]
#[should_panic(expected = "H6: only a settled block")]
fn finalizing_an_unsettled_block_panics() {
    let TrunkAndSide { mut chain, trunk, side, .. } = trunk_and_side();
    chain.depth = FinalityDepth::new(NonZeroU32::MIN);
    for hash in trunk.iter().chain([&side]) {
        if let Some(node) = chain.nodes.get_mut(hash) {
            node.witnesses = Witnesses::default();
        }
    }
    chain.finalize(BlockRef { hash: trunk[5], height: h(5) });
}
