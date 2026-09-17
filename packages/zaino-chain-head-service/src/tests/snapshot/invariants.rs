//! The invariants [`MapBackedSnapshot`] documents, checked against its
//! representation.
//!
//! The property test runs [`check_invariants`] after every move, so a move
//! that breaks an invariant fails at the step that broke it.

use std::collections::HashSet;

use zaino_chain_head::ChainHeadSnapshot as _;
use zaino_primitives::types::{BlockHash, Height};

use crate::snapshot::MapBackedSnapshot;

/// Every retained block's hash, the tip included.
pub(super) fn retained_hashes(graph: &MapBackedSnapshot) -> HashSet<BlockHash> {
    std::iter::once(graph.tip_block().hash())
        .chain(graph.others().keys().copied())
        .collect()
}

/// The first documented invariant that does not hold, if any.
pub(super) fn check_invariants(graph: &MapBackedSnapshot) -> Result<(), String> {
    check_tip_not_in_others(graph)?;
    check_others_keyed_by_hash(graph)?;
    check_canonical_hashes_retained(graph)?;
    check_canonical_chain_ends_at_tip(graph)?;
    check_canonical_chain_is_linked(graph)
}

fn check_tip_not_in_others(graph: &MapBackedSnapshot) -> Result<(), String> {
    let tip = graph.tip_block().hash();
    if graph.others().contains_key(&tip) {
        return Err(format!("others holds the tip {tip}"));
    }
    Ok(())
}

fn check_others_keyed_by_hash(graph: &MapBackedSnapshot) -> Result<(), String> {
    match graph
        .others()
        .iter()
        .find(|(key, block)| **key != block.hash())
    {
        Some((key, block)) => Err(format!(
            "others holds block {} under key {key}",
            block.hash()
        )),
        None => Ok(()),
    }
}

/// `∀ (h, x) ∈ heights_to_hashes: x ∈ retained ∧ height(x) = h`
fn check_canonical_hashes_retained(graph: &MapBackedSnapshot) -> Result<(), String> {
    for (height, hash) in graph.heights_to_hashes() {
        let Some(block) = graph.block_by_hash(hash) else {
            return Err(format!("canonical {hash} at {height} is not retained"));
        };
        if block.height() != *height {
            return Err(format!(
                "canonical {hash} is indexed at {height} but sits at {}",
                block.height()
            ));
        }
    }
    Ok(())
}

/// `max(dom heights_to_hashes) = height(tip) ∧ heights_to_hashes(height(tip)) = tip`
fn check_canonical_chain_ends_at_tip(graph: &MapBackedSnapshot) -> Result<(), String> {
    let tip = graph.tip_block();
    let index = graph.heights_to_hashes();
    let top = index.keys().max();
    if top != Some(&tip.height()) {
        return Err(format!(
            "highest canonical height {top:?} is not the tip's {}",
            tip.height()
        ));
    }
    if index.get(&tip.height()) != Some(&tip.hash()) {
        return Err(format!(
            "the tip {} is not canonical at its height",
            tip.hash()
        ));
    }
    Ok(())
}

/// Heights run without gaps from the lowest to the tip, and each canonical
/// block's parent is the canonical block one height below.
fn check_canonical_chain_is_linked(graph: &MapBackedSnapshot) -> Result<(), String> {
    let index = graph.heights_to_hashes();
    let mut heights: Vec<Height> = index.keys().copied().collect();
    heights.sort_unstable();
    for pair in heights.windows(2) {
        let [below, above] = pair else {
            continue;
        };
        if below.checked_add(1) != Some(*above) {
            return Err(format!("canonical heights skip from {below} to {above}"));
        }
        let parent = graph
            .best_block_by_height(*above)
            .map(|block| block.parent_hash);
        let expected = index.get(below).copied();
        if parent != expected {
            return Err(format!(
                "canonical block at {above} has parent {parent:?}, not {expected:?}"
            ));
        }
    }
    Ok(())
}
