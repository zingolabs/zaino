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
    use std::{num::NonZeroUsize, panic::AssertUnwindSafe, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Tiered};
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{
        CompactCiphertext, Fee, OrchardAction, OrchardData, SaplingData, SaplingOutput,
        Transaction, TransactionId, TransparentData, TreeSize, TreeSizes,
    };
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    /// Three blocks folded one onto the next over an in-memory view: each record = the block
    /// encoded with the running sizes; a parent record claiming near-`u32::MAX` seeds the next
    /// fold (sizes read, not carried), so one block more overflows; a gap or a fork panics
    #[test]
    fn sizes_advance_from_the_parent_record_and_a_non_parent_panics() {
        let network = NetworkType::Regtest;
        let output = SaplingOutput {
            cmu: [1; 32].into(),
            ephemeral_key: [2; 32].into(),
            enc_ciphertext: CompactCiphertext::from([3; CompactCiphertext::LENGTH]),
        };
        let action = OrchardAction {
            nullifier: [4; 32].into(),
            cmx: [5; 32].into(),
            ephemeral_key: [6; 32].into(),
            enc_ciphertext: CompactCiphertext::from([7; CompactCiphertext::LENGTH]),
        };
        // one coinbase committing `(sapling, orchard, ironwood)`
        let txs = |sapling: usize, orchard: usize, ironwood: usize| {
            vec![Transaction {
                txid: TransactionId::from([0xcb; 32]),
                transparent: TransparentData { coinbase: true, ..Default::default() },
                sprout: Default::default(),
                sapling: SaplingData {
                    outputs: vec![output.clone(); sapling],
                    ..Default::default()
                },
                orchard: OrchardData {
                    actions: vec![action.clone(); orchard],
                    ..Default::default()
                },
                ironwood: OrchardData {
                    actions: vec![action.clone(); ironwood],
                    ..Default::default()
                },
            }]
        };
        let fees = |block: &Block| BlockFees {
            height: block.header().height,
            hash: block.header().hash,
            fees: vec![Fee::Coinbase],
        };
        let sizes = |sapling: u32, orchard: u32, ironwood: u32| TreeSizes {
            sapling: TreeSize::from(sapling),
            orchard: TreeSize::from(orchard),
            ironwood: TreeSize::from(ironwood),
        };
        let empty = || {
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/cb"), &schema(network));
            Tiered::new(store.expect("open"), NonZeroUsize::MAX)
        };

        let mut chain = Chain::with_genesis(txs(2, 1, 0));
        let one = chain.mine_with(chain.genesis().hash, txs(3, 0, 4));
        let two = chain.mine_with(one.hash, txs(0, 5, 1));
        let mut tiered = empty();
        for (at, after) in
            [(chain.genesis(), sizes(2, 1, 0)), (one, sizes(5, 1, 4)), (two, sizes(5, 6, 5))]
        {
            let block = chain.block(at.hash);
            let parent = CompactBlockReader::new(tiered.view(), network);
            let changes = fold(&parent, block, &fees(block)).expect("far below u32");
            let records: Vec<&[u8]> = changes.appends(BLOCKS).collect();
            let expected = encode_compact_block(block, &fees(block), &after);
            assert_eq!(records, [&expected[..]], "{at:?}: one record, running sizes");
            assert_eq!(changes.tip(), at);
            tiered.apply(changes);
            assert_eq!(CompactBlockReader::new(tiered.view(), network).tip_sizes(), after);
        }

        // genesis record written claiming sapling = u32::MAX - 1: `one`'s 3 outputs overflow
        let genesis = chain.block(chain.genesis().hash);
        let mut seeded = empty();
        let mut changes = Changes::new(chain.genesis(), &schema(network));
        let near_full = sizes(u32::MAX - 1, 0, 0);
        changes.append(BLOCKS, &encode_compact_block(genesis, &fees(genesis), &near_full));
        seeded.apply(changes);
        let parent = CompactBlockReader::new(seeded.view(), network);
        let block = chain.block(one.hash);
        let overflow = fold(&parent, block, &fees(block)).err();
        assert_eq!(overflow, Some(TreeSizeOutOfRange { got: u64::from(u32::MAX) + 2 }));

        // parents: `tiered` = 0..=2, `seeded` = 0, `through_one` = 0..=1
        let sibling = chain.mine_with(chain.genesis().hash, txs(1, 1, 1));
        let cousin = chain.mine_with(sibling.hash, txs(1, 1, 1));
        let through_one = {
            let mut tiered = empty();
            for at in [chain.genesis(), one] {
                let block = chain.block(at.hash);
                let parent = CompactBlockReader::new(tiered.view(), network);
                tiered.apply(fold(&parent, block, &fees(block)).expect("small"));
            }
            tiered
        };
        for (case, parent, at) in [
            ("gap", &seeded, two),
            ("fork at the same height", &through_one, cousin),
            ("below the tip", &tiered, one),
        ] {
            let parent = CompactBlockReader::new(parent.view(), network);
            let block = chain.block(at.hash);
            let folded =
                std::panic::catch_unwind(AssertUnwindSafe(|| fold(&parent, block, &fees(block))));
            let payload = folded.expect_err(case);
            let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
            assert!(message.contains("does not extend the parent tip"), "{case}: {message}");
        }
    }
}
