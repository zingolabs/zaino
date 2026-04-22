//! Unit tests for Zaino-state::ChainIndex::types and encoding.

use crate::{
    chain_index::{tests::init_tracing, types::EquihashSolution},
    version, BlockContext, BlockData, BlockHeaderData, ZainoVersionedSerde as _,
};

/// Canonical [`BlockHeaderData`] used by the serde tests in this module
/// and by cross-boundary tests that start from its encoded bytes.
///
/// Changing the values produced here invalidates every golden-bytes test
/// that pins an encoding — regenerate goldens and audit the change for
/// on-disk-stability implications.
pub(crate) fn canonical_blockheaderdata() -> BlockHeaderData {
    let hash = crate::BlockHash::from([1u8; 32]);
    let parent_hash = crate::BlockHash::from([2u8; 32]);
    let chainwork = crate::ChainWork::from_u256(0.into());
    let height = crate::Height(42);
    let solution = EquihashSolution::Standard([6u8; 1344]);

    let bctx = BlockContext::new(hash, parent_hash, chainwork, height);
    let bdata = BlockData::new(1, 2, [3u8; 32], [4u8; 32], 3, [5u8; 32], solution);
    BlockHeaderData::new(bctx, bdata)
}

/// Byte-for-byte expected output of
/// `canonical_blockheaderdata().to_bytes()` under the current
/// body-format version (V2).
///
/// Assembled from field-labelled pieces so the layout is self-documenting
/// — each contribution corresponds to exactly one field in the
/// `BlockHeaderData` -> `PersistentBlockContext` + `BlockData` encoding.
/// A change in encoder output will be diffed directly against this
/// reconstruction, and the failing line should point at the offending
/// field.
pub(crate) fn expected_v2_bytes() -> Vec<u8> {
    let mut out = Vec::with_capacity(1565);
    // Outer BlockHeaderData V2 version tag.
    out.push(version::V2);
    // PersistentBlockContext V2 version tag.
    out.push(version::V2);
    // BlockHash (hash): V1 tag + 32-byte body.
    out.push(version::V1);
    out.extend_from_slice(&[0x01; 32]);
    // BlockHash (parent_hash): V1 tag + 32-byte body.
    out.push(version::V1);
    out.extend_from_slice(&[0x02; 32]);
    // ChainWork: V1 tag + U256 big-endian (value = 0).
    out.push(version::V1);
    out.extend_from_slice(&[0x00; 32]);
    // Height: V1 tag + u32 big-endian (value = 42).
    out.push(version::V1);
    out.extend_from_slice(&42u32.to_be_bytes());
    // BlockData V1 tag.
    out.push(version::V1);
    // BlockData.version: u32 little-endian (value = 1).
    out.extend_from_slice(&1u32.to_le_bytes());
    // BlockData.time: i64 little-endian (value = 2).
    out.extend_from_slice(&2i64.to_le_bytes());
    // BlockData.merkle_root: 32 bytes.
    out.extend_from_slice(&[0x03; 32]);
    // BlockData.block_commitments: 32 bytes.
    out.extend_from_slice(&[0x04; 32]);
    // BlockData.bits: u32 little-endian (value = 3).
    out.extend_from_slice(&3u32.to_le_bytes());
    // BlockData.nonce: 32 bytes.
    out.extend_from_slice(&[0x05; 32]);
    // EquihashSolution: Standard variant tag (0x01) + 0x00 padding byte.
    out.extend_from_slice(&[0x01, 0x00]);
    // EquihashSolution::Standard body: 1344 bytes.
    out.extend_from_slice(&[0x06; 1344]);
    out
}

/// Golden-bytes guard for the current [`BlockHeaderData`] body-format version.
///
/// This is the public observable form of the DB-boundary serde — the V2
/// `BlockHeaderData` body embeds `PersistentBlockContext` V2 and
/// `BlockData` V1, preceded by the outer V2 version tag. Failing this test
/// means a feature-layer change (to `BlockIndex`, `BlockContext`,
/// `BlockData`, or any nested field type) silently altered the on-disk
/// encoding.
///
/// If such a change is intentional, introduce a new body-format version
/// (see [`crate::chain_index::encoding::version`]) rather than updating
/// the expected layout in place — an in-place update is an explicit
/// compatibility-break acknowledgement.
#[test]
fn blockheaderdata_v2_golden_bytes() {
    init_tracing();
    let bheader = canonical_blockheaderdata();
    let actual = bheader.to_bytes().expect("v2 to_bytes");
    assert_eq!(
        actual,
        expected_v2_bytes(),
        "BlockHeaderData V2 encoding drifted. \
         If intentional, introduce a new body-format version rather than \
         updating `expected_v2_bytes` in place."
    );
}

#[test]
fn blockheaderdata_v1_v2_serde() {
    init_tracing();

    let bheader = canonical_blockheaderdata();

    // Produce v1 bytes using the versioned encode API (tag + body).
    let v1_bytes = bheader
        .to_bytes_with_version(version::V1)
        .expect("v1 to_bytes_with_version");

    // Parse v1 bytes — should succeed and round-trip.
    let parsed_v1 = BlockHeaderData::from_bytes(&v1_bytes).expect("decode v1 BlockHeaderData");
    assert_eq!(parsed_v1, bheader);

    // Now round-trip v2 (current writer). BlockHeaderData::to_bytes() writes V2.
    let v2_bytes = bheader.to_bytes().expect("v2 to_bytes");
    let parsed_v2 = BlockHeaderData::from_bytes(&v2_bytes).expect("decode v2 BlockHeaderData");
    assert_eq!(parsed_v2, bheader);

    // sanity: v1 and v2 encodings must differ
    assert_ne!(v1_bytes, v2_bytes, "v1 and v2 encodings should differ");
}
