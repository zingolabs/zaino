//! Fire drills: each check in `check()` and each precondition
//! assert, seen firing on a planted bug (a check never seen firing is not known to work)

use std::num::NonZeroU32;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};

use super::{HeaderChain, Node};
use crate::testing::{insert, HeaderViews};
use crate::{decode_header, Inserted};

/// Panic message of `run`, `None` = it returned
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = catch_unwind(AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
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

/// Trunk 0..=12 final through 3 (depth 9), a side header at 12 on 11 (equal work, received after
/// the trunk's 12): every check passes; then one planted bug per check, each firing its own
#[test]
fn every_invariant_check_fires_on_its_planted_bug() {
    let mut builder = MockChain::regtest();
    let tip = builder.mine_empty(12);
    let trunk: Vec<BlockHash> = builder.blocks(tip).iter().map(|b| b.header().hash).collect();
    let side = builder.fork(h(11)).mine_empty(1).tip().hash;
    let valid = || {
        let mut chain = builder.header_chain(ReorgDepth::new(NonZeroU32::new(9).expect("nz")));
        insert(&mut chain, &builder.blocks(tip)).expect("valid");
        insert(&mut chain, &[Arc::clone(builder.block(side))]).expect("valid");
        chain.finalize(chain.finalizable().expect("12 − 9"));
        chain
    };
    assert_eq!(fired(|| valid().check()), None, "the unplanted chain passes");
    assert_eq!(valid().final_tip().map(|tip| u32::from(tip.height)), Some(3));

    type Plant = Box<dyn Fn(&mut HeaderChain)>;
    let at: [BlockHash; 13] = trunk.clone().try_into().expect("genesis ..= 12");
    let drills: Vec<(&str, Plant)> = vec![
        (
            "H1: best = the max-work leaf",
            Box::new(move |c| {
                let side_record = c.nodes[&side].record;
                c.best_path.set(c.best_path.len() - 1, side_record);
            }),
        ),
        (
            "does not descend from the final tip",
            Box::new(move |c| {
                c.nodes.get_mut(&at[4]).expect("held").parent = BlockHash::from([9u8; 32]);
            }),
        ),
        (
            "H2: one height above its parent",
            Box::new(move |c| {
                c.nodes.get_mut(&at[6]).expect("held").height = Height::try_from(9u32).expect("h");
            }),
        ),
        (
            "H2: nothing held before an anchor",
            Box::new(|c| {
                c.finals.clear();
            }),
        ),
        (
            "H2: finals one height apart",
            Box::new(|c| {
                c.finals.remove(1);
            }),
        ),
        (
            "H5: best_path record = its node",
            Box::new(|c| {
                let mut record = c.best_path[2];
                record.time += 1;
                c.best_path.set(2, record);
            }),
        ),
        (
            "H5: best_path at",
            Box::new(|c| {
                c.best_path.remove(2);
            }),
        ),
        (
            "H4: 37 side nodes",
            Box::new(move |c| (1..=36).for_each(|salt| clone_side(c, side, salt))),
        ),
        (
            "H4: 34 side tips",
            Box::new(move |c| (1..=33).for_each(|salt| clone_side(c, side, salt))),
        ),
        (
            "tree: cumulative work",
            Box::new(move |c| {
                c.nodes.get_mut(&side).expect("held").record.cumulative_work += 1;
            }),
        ),
        (
            "tree: child count",
            Box::new(move |c| {
                c.nodes.get_mut(&at[11]).expect("held").children = 1;
            }),
        ),
        (
            "tree: leaf set",
            Box::new(move |c| {
                c.leaves.remove(&side);
            }),
        ),
        (
            "vouched, its parent not",
            Box::new(move |c| {
                c.nodes.get_mut(&at[8]).expect("held").vouched = false;
            }),
        ),
    ];
    for (expected, plant) in drills {
        let mut chain = valid();
        plant(&mut chain);
        let message = fired(|| chain.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: a caller bug panics naming the invariant
    let off_best = BlockRef { hash: side, height: Height::try_from(12u32).expect("h") };
    let shallow = BlockRef { hash: at[5], height: Height::try_from(5u32).expect("h") };
    let preconditions: [(&str, Plant); 3] = [
        ("H2: only a best-branch block becomes final", Box::new(move |c| c.finalize(off_best))),
        (
            "H2: the final boundary stays `depth` below the best",
            Box::new(move |c| c.finalize(shallow)),
        ),
        (
            "H6: only a vouched block",
            Box::new(move |c| {
                c.depth = ReorgDepth::new(NonZeroU32::MIN);
                at.iter().chain([&side]).for_each(|hash| {
                    c.nodes.get_mut(hash).into_iter().for_each(|node| node.vouched = false);
                });
                c.finalize(shallow);
            }),
        ),
    ];
    for (expected, call) in preconditions {
        let mut chain = valid();
        let message = fired(|| call(&mut chain)).unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }
    let mut chain = valid();
    let header = decode_header(&builder.header_bytes(side)).expect("real header");
    assert_eq!(chain.insert(&header), Ok(Inserted::Known), "a valid call passes");
}
