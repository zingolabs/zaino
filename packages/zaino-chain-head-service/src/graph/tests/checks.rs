//! Single moves on a [`ChainGraph`], without a service around them.

use zaino_primitives::types::BlockRef;

use super::{chain_head_block, InspectableGraph};
use crate::{
    graph::{ChainGraph, NotChildOfTip, NotOnBestChain},
    tests::{best_chain_hashes, hash, height},
};

/// `0 -> 1`, with block 1 the tip.
fn two_block_graph<G: ChainGraph>() -> G {
    let mut graph = G::from_initial_block(chain_head_block(0, 0, 0, 1));
    assert_eq!(graph.extend(chain_head_block(1, 1, 0, 2)), Ok(()));
    graph
}

/// Block 0, the base of [`two_block_graph`].
fn base<G: ChainGraph>(graph: &G) -> BlockRef {
    graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference
}

pub(super) fn extending_with_a_block_whose_parent_is_not_the_tip_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let (tip, detached) = (graph.best_tip(), chain_head_block(2, 2, 0, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(2)).is_none());
}

pub(super) fn extending_with_a_block_at_the_wrong_height_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let (tip, detached) = (graph.best_tip(), chain_head_block(3, 2, 1, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(3)).is_none());
}

pub(super) fn trimming_above_the_tip_keeps_the_tip<G: InspectableGraph>() {
    let mut graph = two_block_graph::<G>();

    graph.remove_finalized_blocks(height(100));

    assert_eq!(graph.retained_block_count(), 1);
    assert_eq!(graph.tip_block().hash(), hash(1));
    assert_eq!(best_chain_hashes(&graph), vec![hash(1)]);
}

pub(super) fn rewinding_keeps_the_old_tip_as_a_competing_block<G: InspectableGraph>() {
    let mut graph = two_block_graph::<G>();

    assert_eq!(graph.rewind_to(graph.best_tip()), Ok(()));
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.tip_block().hash(), hash(0));
    let old_tip = graph
        .block_by_hash(&hash(1))
        .expect("the old tip is retained");
    assert!(!graph.is_on_best_chain(old_tip.reference));
    assert_eq!(graph.retained_block_count(), 2);
}

pub(super) fn rewinding_to_a_competing_block_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let old_tip = graph.best_tip();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.rewind_to(old_tip), Err(NotOnBestChain));
    assert_eq!(graph.tip_block().hash(), hash(0));
}

pub(super) fn a_heavier_competing_block_is_the_heaviest<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(1));
}

pub(super) fn the_tip_wins_an_equal_work_tie<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));
    assert_eq!(graph.extend(chain_head_block(1, 11, 0, 2)), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(11));
}
