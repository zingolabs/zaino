//! Zaino ChainIndex::types unit tests.

use crate::{chain_index::tests::init_tracing, version, BlockIndex, ZainoVersionedSerde as _};

#[tokio::test(flavor = "multi_thread")]
async fn blockindex_v1_v2_serde() {
    init_tracing();

    // Build canonical components
    let hash = crate::BlockHash::from([1u8; 32]);
    let parent_hash = crate::BlockHash::from([2u8; 32]);
    let chainwork = crate::ChainWork::from_u256(0.into());
    let height = crate::Height(42);

    // Create a BlockIndex value
    let bidx = BlockIndex::new(hash, parent_hash, chainwork, height);

    // Produce v1 bytes using the new versioned encode API (tag + body)
    let v1_bytes = bidx
        .to_bytes_with_version(version::V1)
        .expect("v1 to_bytes_with_version");

    // Parse v1 bytes using the new BlockIndex deserialiser — should succeed and produce same height.
    let parsed_v1 = BlockIndex::from_bytes(&v1_bytes).expect("decode v1 BlockIndex");
    assert_eq!(parsed_v1, bidx);

    // Now round-trip a v2 BlockIndex (current writer). BlockIndex::to_bytes() writes V2.
    let v2_bytes = bidx.to_bytes().expect("v2 to_bytes");
    let parsed_v2 = BlockIndex::from_bytes(&v2_bytes).expect("decode v2 BlockIndex");
    assert_eq!(parsed_v2, bidx);

    // sanity: v1 and v2 encodings must differ
    assert_ne!(v1_bytes, v2_bytes, "v1 and v2 encodings should differ");
}
