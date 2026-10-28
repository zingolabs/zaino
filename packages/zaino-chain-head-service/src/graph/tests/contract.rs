//! The [`ChainGraph`] invariants, checked through the read traits alone.
//!
//! These hold for every implementation, so they need nothing from inside one.
//! What only a representation can show (entries the reads skip, internal
//! bookkeeping) is checked by
//! [`InspectableGraph::check_representation`](super::InspectableGraph::check_representation).

use zaino_chain_head::ChainHeadBlock;

use crate::graph::ChainGraph;

/// The first contract invariant that does not hold, if any.
pub(super) fn check_contract<G: ChainGraph>(graph: &G) -> Result<(), String> {
    let best_chain: Vec<&ChainHeadBlock> = graph.best_chain().collect();
    check_tip_is_retained_and_canonical(graph)?;
    check_best_chain_ends_at_tip(graph, &best_chain)?;
    check_best_chain_is_linked(&best_chain)?;
    check_best_chain_blocks_are_canonical(graph, &best_chain)?;
    check_heaviest_block_is_retained(graph)
}

fn check_tip_is_retained_and_canonical<G: ChainGraph>(graph: &G) -> Result<(), String> {
    let tip = graph.tip_block().reference;
    if graph.best_tip() != tip {
        return Err(format!(
            "best_tip {:?} is not the tip block {tip:?}",
            graph.best_tip()
        ));
    }
    if graph.block_by_hash(&tip.hash).map(|block| block.reference) != Some(tip) {
        return Err(format!("the tip {tip:?} is not retained"));
    }
    if !graph.is_on_best_chain(tip) {
        return Err(format!("the tip {tip:?} is not canonical"));
    }
    Ok(())
}

/// `last(best_chain) = tip`: nothing above the tip is canonical.
fn check_best_chain_ends_at_tip<G: ChainGraph>(
    graph: &G,
    best_chain: &[&ChainHeadBlock],
) -> Result<(), String> {
    let last = best_chain.last().map(|block| block.reference);
    let tip = graph.tip_block().reference;
    if last != Some(tip) {
        return Err(format!(
            "the best chain ends at {last:?}, not the tip {tip:?}"
        ));
    }
    Ok(())
}

/// Consecutive canonical blocks are one height apart, and each names the one
/// below as its parent.
fn check_best_chain_is_linked(best_chain: &[&ChainHeadBlock]) -> Result<(), String> {
    for pair in best_chain.windows(2) {
        let [below, above] = pair else {
            continue;
        };
        if below.height().checked_add(1) != Some(above.height()) {
            return Err(format!(
                "canonical heights skip from {} to {}",
                below.height(),
                above.height()
            ));
        }
        if above.parent_hash != below.hash() {
            return Err(format!(
                "canonical block {:?} names parent {}, not {}",
                above.reference,
                above.parent_hash,
                below.hash()
            ));
        }
    }
    Ok(())
}

/// Every block the best chain yields is retained and answers as canonical.
fn check_best_chain_blocks_are_canonical<G: ChainGraph>(
    graph: &G,
    best_chain: &[&ChainHeadBlock],
) -> Result<(), String> {
    for block in best_chain {
        let reference = block.reference;
        if !graph.is_on_best_chain(reference) {
            return Err(format!(
                "{reference:?} is in the best chain but not canonical"
            ));
        }
        let at_height = graph
            .best_block_by_height(reference.height)
            .map(|canonical| canonical.hash());
        if at_height != Some(reference.hash) {
            return Err(format!(
                "canonical block at {} is {at_height:?}, not {}",
                reference.height, reference.hash
            ));
        }
        if graph.block_by_hash(&reference.hash).is_none() {
            return Err(format!("canonical {reference:?} is not retained"));
        }
    }
    Ok(())
}

/// The heaviest block is retained and carries at least the tip's work.
fn check_heaviest_block_is_retained<G: ChainGraph>(graph: &G) -> Result<(), String> {
    let heaviest = graph.heaviest_block();
    if graph.block_by_hash(&heaviest.hash()).is_none() {
        return Err(format!(
            "the heaviest block {:?} is not retained",
            heaviest.reference
        ));
    }
    if heaviest.work < graph.tip_block().work {
        return Err(format!(
            "the heaviest block {:?} carries less work than the tip",
            heaviest.reference
        ));
    }
    Ok(())
}
