//! Fire drills: each check in `check()` and each precondition assert, seen firing on a planted bug
//! (a check never seen firing is not known to work)

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{BlockHash, BlockRef, ReorgDepth};

use super::{HeaderChain, Node};
use crate::testing::{insert, HeaderViews};
use crate::{decode_header, Inserted};

/// Depth 9; `trunk[h]` = trunk at height `h`, 0..=12, final through 3; `side` = a header at 12 on
/// 11 (equal work, received after the trunk's 12); `builder` mined both
struct Valid {
    chain: HeaderChain,
    trunk: [BlockHash; 13],
    side: BlockHash,
    builder: MockChain,
}

fn valid() -> Valid {
    let mut builder = MockChain::regtest();
    let tip = builder.mine_empty(12);
    let trunk: Vec<BlockHash> = builder.blocks(tip).iter().map(|b| b.header().hash).collect();
    let side = builder.fork(h(11)).mine_empty(1).tip().hash;
    let mut chain = builder.header_chain(ReorgDepth::new(NonZeroU32::new(9).expect("nz")));
    insert(&mut chain, &builder.blocks(tip)).expect("valid");
    insert(&mut chain, &[Arc::clone(builder.block(side))]).expect("valid");
    chain.finalize(chain.finalizable().expect("12 − 9"));
    let trunk = trunk.try_into().expect("genesis ..= 12");
    Valid { chain, trunk, side, builder }
}

/// Copy of side node `of` under a fresh hash: same parent and work, received last (bookkeeping
/// kept consistent, so only the bound breaks)
fn clone_side(chain: &mut HeaderChain, of: BlockHash, salt: u8) {
    let mut node: Node = chain.nodes[&of];
    let mut hash = <[u8; 32]>::from(of);
    hash[0] ^= salt;
    node.record.hash = BlockHash::from(hash);
    node.received = u64::MAX - u64::from(salt);
    chain.nodes.insert(node.record.hash, node);
    chain.leaves.insert(node.record.hash);
    if let Some(parent) = chain.nodes.get_mut(&node.parent) {
        parent.children += 1;
    }
}

#[test]
fn the_unplanted_chain_passes_every_check_and_knows_its_side_header() {
    let Valid { mut chain, side, builder, .. } = valid();
    chain.check();
    assert_eq!(chain.final_tip().map(|tip| u32::from(tip.height)), Some(3));
    let header = decode_header(&builder.header_bytes(side)).expect("real header");
    assert_eq!(chain.insert(&header), Ok(Inserted::Known));
}

#[test]
#[should_panic(expected = "H1: best = the max-work leaf")]
fn check_catches_a_best_that_is_not_the_max_work_leaf() {
    let Valid { mut chain, side, .. } = valid();
    let side_record = chain.nodes[&side].record;
    chain.best_path.set(chain.best_path.len() - 1, side_record);
    chain.check();
}

#[test]
#[should_panic(expected = "does not descend from the final tip")]
fn check_catches_a_node_off_the_final_tip() {
    let Valid { mut chain, trunk, .. } = valid();
    chain.nodes.get_mut(&trunk[4]).expect("held").parent = BlockHash::from([9u8; 32]);
    chain.check();
}

#[test]
#[should_panic(expected = "H2: one height above its parent")]
fn check_catches_a_height_gap() {
    let Valid { mut chain, trunk, .. } = valid();
    chain.nodes.get_mut(&trunk[6]).expect("held").height = h(9);
    chain.check();
}

#[test]
#[should_panic(expected = "H2: nothing held before an anchor")]
fn check_catches_a_chain_without_an_anchor() {
    let Valid { mut chain, .. } = valid();
    chain.finals.clear();
    chain.check();
}

#[test]
#[should_panic(expected = "H2: finals one height apart")]
fn check_catches_a_gap_in_the_finals() {
    let Valid { mut chain, .. } = valid();
    chain.finals.remove(1);
    chain.check();
}

#[test]
#[should_panic(expected = "H5: best_path record = its node")]
fn check_catches_a_best_path_record_off_its_node() {
    let Valid { mut chain, .. } = valid();
    let mut record = chain.best_path[2];
    record.time += 1;
    chain.best_path.set(2, record);
    chain.check();
}

#[test]
#[should_panic(expected = "H5: best_path at")]
fn check_catches_a_best_path_missing_a_height() {
    let Valid { mut chain, .. } = valid();
    chain.best_path.remove(2);
    chain.check();
}

#[test]
#[should_panic(expected = "H4: 37 side nodes")]
fn check_catches_side_nodes_past_the_bound() {
    let Valid { mut chain, side, .. } = valid();
    (1..=36).for_each(|salt| clone_side(&mut chain, side, salt));
    chain.check();
}

#[test]
#[should_panic(expected = "H4: 34 side tips")]
fn check_catches_side_tips_past_the_bound() {
    let Valid { mut chain, side, .. } = valid();
    (1..=33).for_each(|salt| clone_side(&mut chain, side, salt));
    chain.check();
}

#[test]
#[should_panic(expected = "tree: cumulative work")]
fn check_catches_cumulative_work_off_its_parent() {
    let Valid { mut chain, side, .. } = valid();
    chain.nodes.get_mut(&side).expect("held").record.cumulative_work += 1;
    chain.check();
}

#[test]
#[should_panic(expected = "tree: child count")]
fn check_catches_a_wrong_child_count() {
    let Valid { mut chain, trunk, .. } = valid();
    chain.nodes.get_mut(&trunk[11]).expect("held").children = 1;
    chain.check();
}

#[test]
#[should_panic(expected = "tree: leaf set")]
fn check_catches_a_leaf_missing_from_the_leaf_set() {
    let Valid { mut chain, side, .. } = valid();
    chain.leaves.remove(&side);
    chain.check();
}

#[test]
#[should_panic(expected = "vouched, its parent not")]
fn check_catches_a_vouched_node_under_an_unvouched_parent() {
    let Valid { mut chain, trunk, .. } = valid();
    chain.nodes.get_mut(&trunk[8]).expect("held").vouched = false;
    chain.check();
}

#[test]
#[should_panic(expected = "H2: only a best-branch block becomes final")]
fn finalizing_a_block_off_the_best_branch_panics() {
    let Valid { mut chain, side, .. } = valid();
    chain.finalize(BlockRef { hash: side, height: h(12) });
}

#[test]
#[should_panic(expected = "H2: the final boundary stays `depth` below the best")]
fn finalizing_within_depth_of_the_best_panics() {
    let Valid { mut chain, trunk, .. } = valid();
    chain.finalize(BlockRef { hash: trunk[5], height: h(5) });
}

#[test]
#[should_panic(expected = "H6: only a vouched block")]
fn finalizing_an_unvouched_block_panics() {
    let Valid { mut chain, trunk, side, .. } = valid();
    chain.depth = ReorgDepth::new(NonZeroU32::MIN);
    for hash in trunk.iter().chain([&side]) {
        if let Some(node) = chain.nodes.get_mut(hash) {
            node.vouched = false;
        }
    }
    chain.finalize(BlockRef { hash: trunk[5], height: h(5) });
}
