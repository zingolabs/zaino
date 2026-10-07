//! Block + its fees → its one record (pure: parent state read through its reader, never carried)
//!
//! - sizes after `h` = parent tip record's `chainMetadata` + what `h` commits ([`Block`] carries
//!   none; `z_gettreestate` = one round trip per block, unaffordable in a full sync)
//! - folded onto anything but its parent = every later record silently mis-sized → panic

use zaino_persistence::{Changes, SequenceRead};
use zaino_primitives::types::{Block, BlockFees, BlockRef, Height, TreeSizeOutOfRange};

use crate::{encode_compact_block, schema, CompactBlockReader, BLOCKS};

/// `Err` = a tree past `u32` (#549)
pub fn fold<V: SequenceRead>(
    parent: &CompactBlockReader<V>,
    block: &Block,
    fees: &BlockFees,
) -> Result<Changes, TreeSizeOutOfRange> {
    let header = block.header();
    let at = BlockRef { hash: header.hash, height: header.height };
    let tip = parent.tip();
    let extends = match tip {
        None => at.height == Height::GENESIS,
        Some(tip) => tip.height.next() == at.height && tip.hash == header.prev_hash,
    };
    assert!(extends, "compact_block: block {at:?} does not extend the parent tip {tip:?}");

    let sizes = parent.tip_sizes().advance(block)?;
    let mut changes = Changes::new(at, &schema(parent.network()));
    changes.append(BLOCKS, &encode_compact_block(block, fees, &sizes));
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use std::{panic::AssertUnwindSafe, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store};
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::{TreeSize, TreeSizes};
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    /// Genesis + three blocks folded one onto the next over an in-memory view: each record = the
    /// block encoded with the running sizes; a parent record claiming near-`u32::MAX` seeds the
    /// next fold (sizes read, not carried), so one block more overflows; a gap or a fork panics
    #[test]
    fn sizes_advance_from_the_parent_record_and_a_non_parent_panics() {
        let network = NetworkType::Regtest;
        let sizes = |sapling: u32, orchard: u32, ironwood: u32| TreeSizes {
            sapling: TreeSize::from(sapling),
            orchard: TreeSize::from(orchard),
            ironwood: TreeSize::from(ironwood),
        };
        let empty = || {
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/cb"), &schema(network));
            store.expect("open")
        };

        // coinbases commit (sapling, orchard, ironwood) = (2, 1, 0), (3, 0, 4), (0, 5, 1)
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| {
            b.coinbase(|c| c.sapling_output(1).sapling_output(2).orchard_action([1; 32], 1))
        });
        let two = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.sapling_output(3).sapling_output(4).sapling_output(5);
                let c = c.ironwood_action([1; 32], 1).ironwood_action([2; 32], 2);
                c.ironwood_action([3; 32], 3).ironwood_action([4; 32], 4)
            })
        });
        let three = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.orchard_action([2; 32], 2).orchard_action([3; 32], 3);
                let c = c.orchard_action([4; 32], 4).orchard_action([5; 32], 5);
                c.orchard_action([6; 32], 6).ironwood_action([5; 32], 5)
            })
        });
        // height 3 on a sibling of `two`
        let cousin = chain.fork(h(1)).mine_empty(2).tip();
        let fees = |block: &Block| chain.fees(block.header().hash);
        let mut through_three = empty();
        for (at, after) in [
            (chain.genesis(), sizes(0, 0, 0)),
            (one, sizes(2, 1, 0)),
            (two, sizes(5, 1, 4)),
            (three, sizes(5, 6, 5)),
        ] {
            let block = chain.block(at.hash);
            let parent = CompactBlockReader::new(through_three.staged(), network);
            let changes = fold(&parent, block, &fees(block)).expect("far below u32");
            let records: Vec<&[u8]> = changes.appends(BLOCKS).collect();
            let expected = encode_compact_block(block, &fees(block), &after);
            assert_eq!(records, [&expected[..]], "{at:?}: one record, running sizes");
            assert_eq!(changes.tip(), at);
            through_three.apply(changes);
            let reader = CompactBlockReader::new(through_three.staged(), network);
            assert_eq!(reader.tip_sizes(), after);
        }

        // `one`'s record written claiming sapling = u32::MAX - 1: `two`'s 3 outputs overflow
        let mut seeded = empty();
        let genesis = chain.block(chain.genesis().hash);
        let parent = CompactBlockReader::new(seeded.staged(), network);
        seeded.apply(fold(&parent, genesis, &fees(genesis)).expect("bare"));
        let mut changes = Changes::new(one, &schema(network));
        let near_full = sizes(u32::MAX - 1, 0, 0);
        let block = chain.block(one.hash);
        changes.append(BLOCKS, &encode_compact_block(block, &fees(block), &near_full));
        seeded.apply(changes);
        let parent = CompactBlockReader::new(seeded.staged(), network);
        let block = chain.block(two.hash);
        let overflow = fold(&parent, block, &fees(block)).err();
        assert_eq!(overflow, Some(TreeSizeOutOfRange { got: u64::from(u32::MAX) + 2 }));

        // parents: `through_three` = 0..=3, `seeded` = 0..=1, `through_two` = 0..=2
        let through_two = {
            let mut store = empty();
            for at in [chain.genesis(), one, two] {
                let block = chain.block(at.hash);
                let parent = CompactBlockReader::new(store.staged(), network);
                store.apply(fold(&parent, block, &fees(block)).expect("small"));
            }
            store
        };
        for (case, parent, at) in [
            ("gap", &seeded, three),
            ("fork at the same height", &through_two, cousin),
            ("below the tip", &through_three, two),
        ] {
            let parent = CompactBlockReader::new(parent.staged(), network);
            let block = chain.block(at.hash);
            let folded =
                std::panic::catch_unwind(AssertUnwindSafe(|| fold(&parent, block, &fees(block))));
            let payload = folded.expect_err(case);
            let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
            assert!(message.contains("does not extend the parent tip"), "{case}: {message}");
        }
    }
}
