//! Unit tests for Zaino-state::ChainIndex::types and encoding.

use core::num::NonZeroU128;

use crate::codec::DbCodec as _;
use crate::types::{
    AbsoluteChainWork, BlockContext, BlockData, BlockHeaderData, CompactDifficulty,
    EquihashSolution,
};

/// A valid nBits value for test fixtures. Passes zebra's compact difficulty
/// validation but does not correspond to any specific real-world block.
const TEST_VALID_NBITS: u32 = 0x2007_ffff;

/// The chainwork of the canonical fixture header.
const CANONICAL_CHAINWORK: NonZeroU128 = NonZeroU128::new(0x42).expect("nonzero literal");

/// Canonical [`BlockHeaderData`] used by the serde tests in this module
/// and by cross-boundary tests that start from its encoded bytes.
///
/// Changing the values produced here invalidates every golden-bytes test
/// that pins an encoding — regenerate goldens and audit the change for
/// on-disk-stability implications.
pub(crate) fn canonical_blockheaderdata() -> BlockHeaderData<AbsoluteChainWork> {
    let hash = crate::types::BlockHash::from([1u8; 32]);
    let parent_hash = crate::types::BlockHash::from([2u8; 32]);
    let chainwork = AbsoluteChainWork::new(CANONICAL_CHAINWORK);
    let height = crate::types::Height(42);
    let solution = EquihashSolution::Standard([6u8; 1344]);
    let bits = CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid nBits");

    let bctx = BlockContext::new(hash, parent_hash, chainwork, height);
    let bdata = BlockData {
        version: 1,
        time: 2,
        merkle_root: [3u8; 32],
        block_commitments: [4u8; 32],
        bits,
        nonce: [5u8; 32],
        solution,
    };
    BlockHeaderData::new(bctx, bdata)
}

/// Byte-for-byte expected output of `canonical_blockheaderdata().to_bytes()`, assembled field by field so a failure points at the offending field.
pub(crate) fn expected_header_bytes() -> Vec<u8> {
    let mut out = Vec::with_capacity(1557);
    // BlockHash (hash): 32 bytes.
    out.extend_from_slice(&[0x01; 32]);
    // BlockHash (parent_hash): 32 bytes.
    out.extend_from_slice(&[0x02; 32]);
    // AbsoluteChainWork: 32-byte big-endian (value = 0x42, in the low-order 16 bytes). The
    // stored format is big-endian — #1313 once minted this golden little-endian, which is
    // exactly a golden enshrining the bug it should have caught.
    {
        let mut cw_bytes = [0u8; 32];
        cw_bytes[16..].copy_from_slice(&0x42u128.to_be_bytes());
        out.extend_from_slice(&cw_bytes);
    }
    // Height: u32 big-endian (value = 42).
    out.extend_from_slice(&42u32.to_be_bytes());
    // BlockData.version: u32 little-endian (value = 1).
    out.extend_from_slice(&1u32.to_le_bytes());
    // BlockData.time: i64 little-endian (value = 2).
    out.extend_from_slice(&2i64.to_le_bytes());
    // BlockData.merkle_root: 32 bytes.
    out.extend_from_slice(&[0x03; 32]);
    // BlockData.block_commitments: 32 bytes.
    out.extend_from_slice(&[0x04; 32]);
    // BlockData.bits: u32 little-endian (value = 0x2007_ffff, Zcash mainnet genesis nBits).
    out.extend_from_slice(&0x2007_ffffu32.to_le_bytes());
    // BlockData.nonce: 32 bytes.
    out.extend_from_slice(&[0x05; 32]);
    // EquihashSolution: Standard variant tag (0x00).
    out.push(0x00);
    // EquihashSolution::Standard body: 1344 bytes.
    out.extend_from_slice(&[0x06; 1344]);
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

#[test]
fn blockheaderdata_round_trips() {
    let bheader = canonical_blockheaderdata();
    let bytes = bheader.to_bytes().expect("to_bytes");
    let parsed = BlockHeaderData::from_bytes(&bytes).expect("decode BlockHeaderData");
    assert_eq!(parsed, bheader);
}
