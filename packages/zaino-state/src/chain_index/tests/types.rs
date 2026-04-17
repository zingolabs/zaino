//! Unit tests for Zaino-state::ChainIndex::types and encoding.

use crate::{
    chain_index::{tests::init_tracing, types::EquihashSolution},
    version, BlockData, BlockHeaderData, ChainBlock, ZainoVersionedSerde as _,
};

#[tokio::test(flavor = "multi_thread")]
async fn blockindex_v1_v2_serde() {
    init_tracing();

    // Build canonical components
    let hash = crate::BlockHash::from([1u8; 32]);
    let parent_hash = crate::BlockHash::from([2u8; 32]);
    let chainwork = crate::ChainWork::from_u256(0.into());
    let height = crate::Height(42);

    // Create a ChainBlock value
    let chain_block = ChainBlock::new(
        crate::chain_index::non_finalised_state::BlockIndex { height, hash: hash },
        parent_hash,
        chainwork,
    );

    // Produce v1 bytes using the new versioned encode API (tag + body)
    let v1_bytes = chain_block
        .to_bytes_with_version(version::V1)
        .expect("v1 to_bytes_with_version");

    // Parse v1 bytes using the new ChainBlock deserialiser — should succeed and produce same height.
    let parsed_v1 = ChainBlock::from_bytes(&v1_bytes).expect("decode v1 ChainBlock");
    assert_eq!(parsed_v1, chain_block);

    // Now round-trip a v2 ChainBlock (current writer). ChainBlock::to_bytes() writes V2.
    let v2_bytes = chain_block.to_bytes().expect("v2 to_bytes");
    let parsed_v2 = ChainBlock::from_bytes(&v2_bytes).expect("decode v2 ChainBlock");
    assert_eq!(parsed_v2, chain_block);

    // sanity: v1 and v2 encodings must differ
    assert_ne!(v1_bytes, v2_bytes, "v1 and v2 encodings should differ");
}

#[tokio::test(flavor = "multi_thread")]
async fn blockheaderdata_v1_v2_serde() {
    init_tracing();

    // Build canonical components
    let hash = crate::BlockHash::from([1u8; 32]);
    let parent_hash = crate::BlockHash::from([2u8; 32]);
    let chainwork = crate::ChainWork::from_u256(0.into());
    let height = crate::Height(42);
    let solution = EquihashSolution::Standard([6u8; 1344]);

    // Create a ChainBlock value
    let chain_block = ChainBlock::new(
        crate::chain_index::non_finalised_state::BlockIndex { height, hash: hash },
        parent_hash,
        chainwork,
    );

    // create BlockData value
    let bdata = BlockData::new(1, 2, [3u8; 32], [4u8; 32], 3, [5u8; 32], solution);

    // Create BlockHeader value:
    let bheader = BlockHeaderData::new(chain_block, bdata);

    // Produce v1 bytes using the new versioned encode API (tag + body):w
    let v1_bytes = bheader
        .to_bytes_with_version(version::V1)
        .expect("v1 to_bytes_with_version");

    // Parse v1 bytes using the new ChainBlock deserialiser — should succeed and produce same height.
    let parsed_v1 = BlockHeaderData::from_bytes(&v1_bytes).expect("decode v1 BlockHeaderData");
    assert_eq!(parsed_v1, bheader);

    // Now round-trip a v2 ChainBlock (current writer). ChainBlock::to_bytes() writes V2.
    let v2_bytes = bheader.to_bytes().expect("v2 to_bytes");
    let parsed_v2 = BlockHeaderData::from_bytes(&v2_bytes).expect("decode v2 BlockHeaderData");
    assert_eq!(parsed_v2, bheader);

    // sanity: v1 and v2 encodings must differ
    assert_ne!(v1_bytes, v2_bytes, "v1 and v2 encodings should differ");
}
