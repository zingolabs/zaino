//! Graph moves on [`MapBackedSnapshot`], without a service around them.
//!
//! The graph always holds its tip, so every question below has an answer
//! whatever has been trimmed or rewound.

use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot as _, ChainHeadWork};
use zaino_primitives::types::TreeRoots;

use super::{best_chain_hashes, block, hash, height};
use crate::snapshot::{MapBackedSnapshot, NotChildOfTip, NotOnBestChain};

mod invariants;
mod properties;

/// A retained block carrying `work`, independent of its parent's.
fn chain_head_block(h: u32, id: u16, parent: u16, work: u128) -> ChainHeadBlock {
    let block = block(h, id, parent);
    ChainHeadBlock {
        reference: zaino_primitives::types::BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work: ChainHeadWork::anchored_at(work),
        block,
        tree_roots: TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        },
    }
}

/// `0 -> 1`, with block 1 the tip.
fn two_block_graph() -> MapBackedSnapshot {
    let mut graph = MapBackedSnapshot::from_initial_block(chain_head_block(0, 0, 0, 1));
    assert_eq!(graph.extend(chain_head_block(1, 1, 0, 2)), Ok(()));
    graph
}

#[test]
fn extending_with_a_block_whose_parent_is_not_the_tip_is_refused() {
    let mut graph = two_block_graph();
    let (tip, detached) = (graph.best_tip(), chain_head_block(2, 2, 0, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(2)).is_none());
}

#[test]
fn extending_with_a_block_at_the_wrong_height_is_refused() {
    let mut graph = two_block_graph();
    let (tip, detached) = (graph.best_tip(), chain_head_block(3, 2, 1, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(3)).is_none());
}

#[test]
fn trimming_above_the_tip_keeps_the_tip() {
    let mut graph = two_block_graph();

    graph.remove_finalized_blocks(height(100));

    assert_eq!(graph.retained_block_count(), 1);
    assert_eq!(graph.tip_block().hash(), hash(1));
    assert_eq!(best_chain_hashes(&graph), vec![hash(1)]);
}

#[test]
fn rewinding_keeps_the_old_tip_as_a_competing_block() {
    let mut graph = two_block_graph();

    assert_eq!(graph.rewind_to(graph.best_tip()), Ok(()));
    let base = graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference;
    assert_eq!(graph.rewind_to(base), Ok(()));

    assert_eq!(graph.tip_block().hash(), hash(0));
    let old_tip = graph
        .block_by_hash(&hash(1))
        .expect("the old tip is retained");
    assert!(!graph.is_on_best_chain(old_tip.reference));
    assert_eq!(graph.retained_block_count(), 2);
}

#[test]
fn rewinding_to_a_competing_block_is_refused() {
    let mut graph = two_block_graph();
    let old_tip = graph.best_tip();
    let base = graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference;
    assert_eq!(graph.rewind_to(base), Ok(()));

    assert_eq!(graph.rewind_to(old_tip), Err(NotOnBestChain));
    assert_eq!(graph.tip_block().hash(), hash(0));
}

#[test]
fn a_heavier_competing_block_is_the_heaviest() {
    let mut graph = two_block_graph();
    let base = graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference;
    assert_eq!(graph.rewind_to(base), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(1));
}

#[test]
fn the_tip_wins_an_equal_work_tie() {
    let mut graph = two_block_graph();
    let base = graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference;
    assert_eq!(graph.rewind_to(base), Ok(()));
    assert_eq!(graph.extend(chain_head_block(1, 11, 0, 2)), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(11));
}
