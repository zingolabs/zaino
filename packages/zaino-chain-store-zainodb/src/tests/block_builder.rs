//! The zebra-block builder's chainwork rule, checked against the block vectors.

use crate::tests::fixtures::vector_network;
use crate::tests::vectors::{load_vector_blocks, VectorBlock};
use crate::types::{
    AbsoluteChainWork, BlockMetadata, BlockWithMetadata, CompactDifficulty, IndexedBlock,
};

fn build(vector: &VectorBlock, parent_chainwork: Option<AbsoluteChainWork>) -> IndexedBlock {
    let metadata = BlockMetadata {
        sapling_root: vector.sapling_root,
        sapling_size: vector.sapling_tree_size as u32,
        orchard_root: vector.orchard_root,
        orchard_size: vector.orchard_tree_size as u32,
        ironwood: None,
        parent_chainwork,
        network: vector_network(),
    };
    IndexedBlock::try_from(BlockWithMetadata::new(&vector.zebra_block, metadata))
        .expect("vector blocks are valid")
}

fn own_work(vector: &VectorBlock) -> AbsoluteChainWork {
    let bits = CompactDifficulty::try_from_be_bytes(
        vector
            .zebra_block
            .header
            .difficulty_threshold
            .bytes_in_display_order(),
    )
    .expect("vector blocks carry valid nBits");
    AbsoluteChainWork::genesis(bits.to_work())
}

/// A block above genesis built with no parent chainwork carries none, rather than its own work as if it were genesis.
#[test]
fn a_block_above_genesis_without_a_parent_chainwork_carries_none() {
    let blocks = load_vector_blocks().expect("the block vectors are present");
    let vector = &blocks[1];
    assert_eq!(vector.height, 1);

    let block = build(vector, None);

    assert!(
        block.context.chainwork.is_none(),
        "a fabricated chainwork would equal the block's own work"
    );
}

/// Genesis built with no parent chainwork carries its own work.
#[test]
fn genesis_without_a_parent_chainwork_carries_its_own_work() {
    let blocks = load_vector_blocks().expect("the block vectors are present");
    let vector = &blocks[0];
    assert_eq!(vector.height, 0);

    assert!(build(vector, None).context.chainwork == Some(own_work(vector)));
}

/// A block built on its parent's chainwork accumulates its own work onto it.
#[test]
fn a_block_with_a_parent_chainwork_accumulates() {
    let blocks = load_vector_blocks().expect("the block vectors are present");

    let genesis = build(&blocks[0], None).context.chainwork;
    let next = build(&blocks[1], genesis).context.chainwork;

    assert!(next > genesis);
}
