//! Zaino ChainIndex::types unit tests.

use crate::{
    chain_index::tests::init_tracing, version, write_option, BlockIndex, ZainoVersionedSerde as _,
};

#[tokio::test(flavor = "multi_thread")]
async fn blockindex_v1_v2_serde() {
    init_tracing();

    // Build canonical components
    let hash = crate::BlockHash::from([1u8; 32]);
    let parent_hash = crate::BlockHash::from([2u8; 32]);
    let chainwork = crate::ChainWork::from_u256(0.into());
    let height = crate::Height(42);

    // Construct a v1-encoded BlockIndex bytes (tag 0x01 + body with Option<Height>)
    let mut v1_bytes: Vec<u8> = Vec::new();
    v1_bytes.push(version::V1); // leading tag for BlockIndex v1
    hash.serialize(&mut v1_bytes).unwrap();
    parent_hash.serialize(&mut v1_bytes).unwrap();
    chainwork.serialize(&mut v1_bytes).unwrap();
    // v1 used Option<Height>
    write_option(&mut v1_bytes, &Some(height), |w, h| h.serialize(w)).unwrap();

    // Parse v1 bytes using the new BlockIndex deserialiser — should succeed and produce same height.
    let parsed_v1 = BlockIndex::from_bytes(&v1_bytes).expect("decode v1 BlockIndex");
    assert_eq!(parsed_v1.height(), height);

    // Now round-trip a v2 BlockIndex (current writer). BlockIndex::to_bytes() writes V2.
    let bidx = BlockIndex::new(hash, parent_hash, chainwork, height);
    let v2_bytes = bidx.to_bytes().expect("v2 to_bytes");
    let parsed_v2 = BlockIndex::from_bytes(&v2_bytes).expect("decode v2 BlockIndex");
    assert_eq!(parsed_v2, bidx);
}
