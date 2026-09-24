//! Unit tests for Zaino-state::ChainIndex::types and encoding.

use crate::codec::DbCodec as _;
use crate::store::finalised_source::v1::schema::canonical;
use crate::types::{AbsoluteChainWork, BlockHeaderData};

/// The schema's canonical [`BlockHeaderData`], which is also what the serde tests here and the cross-boundary tests encode, so one instance pins the encoding for the goldens and the schema hash alike.
pub(crate) fn canonical_blockheaderdata() -> BlockHeaderData<AbsoluteChainWork> {
    canonical::block_header_data()
}

/// Byte-for-byte expected output of `canonical_blockheaderdata().to_bytes()`, assembled field by field so a failure points at the offending field.
pub(crate) fn expected_header_bytes() -> Vec<u8> {
    let mut out = Vec::with_capacity(249);
    // BlockHash (hash): 32 bytes.
    out.extend_from_slice(&[0x11; 32]);
    // BlockHash (parent_hash): 32 bytes.
    out.extend_from_slice(&[0x99; 32]);
    // AbsoluteChainWork: 32-byte big-endian (value = 0x0dec_0de0, in the low-order 16 bytes).
    // The stored format is big-endian — #1313 once minted this golden little-endian, which is
    // exactly a golden enshrining the bug it should have caught.
    {
        let mut cw_bytes = [0u8; 32];
        cw_bytes[16..].copy_from_slice(&0x0dec_0de0u128.to_be_bytes());
        out.extend_from_slice(&cw_bytes);
    }
    // Height: u32 big-endian (value = 123_456).
    out.extend_from_slice(&123_456u32.to_be_bytes());
    // BlockData.version: u32 little-endian (value = 4).
    out.extend_from_slice(&4u32.to_le_bytes());
    // BlockData.time: i64 little-endian (value = 0x6543_2100).
    out.extend_from_slice(&0x6543_2100i64.to_le_bytes());
    // BlockData.merkle_root: 32 bytes.
    out.extend_from_slice(&[0x66; 32]);
    // BlockData.block_commitments: 32 bytes.
    out.extend_from_slice(&[0x77; 32]);
    // BlockData.bits: u32 little-endian (value = 0x2007_ffff, Zcash mainnet genesis nBits).
    out.extend_from_slice(&0x2007_ffffu32.to_le_bytes());
    // BlockData.nonce: 32 bytes.
    out.extend_from_slice(&[0x88; 32]);
    // EquihashSolution: Regtest variant tag (0x01).
    out.push(0x01);
    // EquihashSolution::Regtest body: 36 bytes.
    out.extend_from_slice(&[0x55; 36]);
    out
}

/// A failure means a change to `BlockIndex`, `BlockContext`, `BlockData` or a nested field altered the stored header encoding, which forces every database to rebuild.
#[test]
fn blockheaderdata_golden_bytes() {
    let bheader = canonical_blockheaderdata();
    let actual = bheader.to_bytes().expect("to_bytes");
    assert_eq!(
        actual,
        expected_header_bytes(),
        "BlockHeaderData encoding drifted; if intentional, update this golden and the schema hash golden"
    );
}

/// One canonical header pins the encoding for both the goldens here and the schema hash, so the two cannot drift apart.
#[test]
fn the_fixture_header_is_the_schema_canonical_header() {
    assert_eq!(
        canonical_blockheaderdata(),
        crate::store::finalised_source::v1::schema::canonical::block_header_data()
    );
}

#[test]
fn blockheaderdata_round_trips() {
    let bheader = canonical_blockheaderdata();
    let bytes = bheader.to_bytes().expect("to_bytes");
    let parsed = BlockHeaderData::from_bytes(&bytes).expect("decode BlockHeaderData");
    assert_eq!(parsed, bheader);
}
