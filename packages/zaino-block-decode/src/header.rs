//! The block header: its fixed fields, its hash, and the height its coinbase
//! commits to.

use zaino_primitives::types::{
    BlockCommitments, BlockHash, BlockHeader, CompactDifficulty, EquihashSolution, Height,
    MerkleRoot,
};

use crate::error::DecodeError;
use crate::reader::Reader;
use crate::txid::sha256d;

/// The standard equihash solution length (n = 200, k = 9).
const STANDARD_SOLUTION: usize = 1344;
/// The regtest equihash solution length (n = 48, k = 5).
const REGTEST_SOLUTION: usize = 36;

/// A header as encoded, plus the bytes it was read from (its hash is the
/// double-SHA256 of exactly those).
pub(crate) struct RawHeader<'a> {
    pub(crate) version: i32,
    pub(crate) prev_hash: [u8; 32],
    pub(crate) merkle_root: [u8; 32],
    pub(crate) commitments: [u8; 32],
    pub(crate) time: u32,
    pub(crate) bits: u32,
    pub(crate) nonce: [u8; 32],
    pub(crate) solution: &'a [u8],
    pub(crate) bytes: &'a [u8],
}

pub(crate) fn read_header<'a>(reader: &mut Reader<'a>) -> Result<RawHeader<'a>, DecodeError> {
    let start = reader.position();
    let version = reader.i32_le()?;
    let prev_hash = reader.array()?;
    let merkle_root = reader.array()?;
    let commitments = reader.array()?;
    let time = reader.u32_le()?;
    let bits = reader.u32_le()?;
    let nonce = reader.array()?;
    let solution = reader.var_bytes()?;
    Ok(RawHeader {
        version,
        prev_hash,
        merkle_root,
        commitments,
        time,
        bits,
        nonce,
        solution,
        bytes: reader.since(start),
    })
}

pub(crate) fn project_header(
    raw: &RawHeader<'_>,
    height: Height,
) -> Result<BlockHeader, DecodeError> {
    let unexpected = || DecodeError::Solution(raw.solution.len());
    let solution = match raw.solution.len() {
        STANDARD_SOLUTION => {
            EquihashSolution::Standard(raw.solution.try_into().map_err(|_| unexpected())?)
        }
        REGTEST_SOLUTION => {
            EquihashSolution::Regtest(raw.solution.try_into().map_err(|_| unexpected())?)
        }
        _ => return Err(unexpected()),
    };
    Ok(BlockHeader {
        hash: BlockHash::from(sha256d(raw.bytes)),
        version: u32::try_from(raw.version)
            .map_err(|_| DecodeError::NegativeHeaderVersion(raw.version))?,
        prev_hash: BlockHash::from(raw.prev_hash),
        height,
        time: raw.time,
        merkle_root: MerkleRoot::from(raw.merkle_root),
        block_commitments: BlockCommitments::from(raw.commitments),
        bits: CompactDifficulty::try_from_bits(raw.bits)?,
        nonce: raw.nonce,
        solution,
    })
}

/// The height a block's coinbase commits to (BIP 34), or genesis.
///
/// Genesis predates the rule: its coinbase carries no height, and it is the
/// one block whose previous hash is all zeros. Every later coinbase script
/// opens with a canonical script-number push of the height: `OP_1`–`OP_16`
/// for 1 to 16, otherwise a length byte and the minimal little-endian bytes.
/// A push in any other form — a longer encoding, a sign byte where none is
/// needed — is rejected, as zebra rejects it.
pub(crate) fn coinbase_height(
    prev_hash: &[u8; 32],
    script_sig: &[u8],
) -> Result<Height, DecodeError> {
    if *prev_hash == [0u8; 32] {
        return Ok(Height::GENESIS);
    }
    let (height, push_len) = match *script_sig
        .first()
        .ok_or(DecodeError::CoinbaseHeight("empty coinbase script"))?
    {
        op_n @ 0x51..=0x60 => (u64::from(op_n - 0x50), 1),
        len @ 1..=5 => {
            let digits = script_sig
                .get(1..=usize::from(len))
                .ok_or(DecodeError::CoinbaseHeight("height push truncated"))?;
            let mut le = [0u8; 8];
            le[..digits.len()].copy_from_slice(digits);
            (u64::from_le_bytes(le), 1 + digits.len())
        }
        _ => {
            return Err(DecodeError::CoinbaseHeight(
                "script does not open with a height push",
            ));
        }
    };
    if script_sig[..push_len] != canonical_push(height) {
        return Err(DecodeError::CoinbaseHeight("height push is not canonical"));
    }
    let height = u32::try_from(height)
        .map_err(|_| DecodeError::CoinbaseHeight("height exceeds the 32-bit range"))?;
    Ok(Height::try_from(height)?)
}

/// The script-number push a canonical coinbase makes for `height`: `OP_n`
/// for 1 to 16, else the minimal little-endian magnitude behind a length
/// byte, with a zero byte appended when the top bit would read as a sign.
fn canonical_push(height: u64) -> Vec<u8> {
    if (1..=16).contains(&height) {
        return vec![u8::try_from(0x50 + height).expect("0x51..=0x60 fits a byte")];
    }
    let mut digits: Vec<u8> = height.to_le_bytes().to_vec();
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.last().is_some_and(|top| top & 0x80 != 0) {
        digits.push(0);
    }
    let mut push = vec![u8::try_from(digits.len()).expect("at most nine digits")];
    push.extend(digits);
    push
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOT_GENESIS: [u8; 32] = [1u8; 32];

    fn height_of(script: &[u8]) -> Result<u32, DecodeError> {
        coinbase_height(&NOT_GENESIS, script).map(u32::from)
    }

    #[test]
    fn genesis_has_height_zero_whatever_its_script_says() {
        let height = coinbase_height(&[0u8; 32], &[0x04, 0xff, 0xff, 0x00, 0x1d]).expect("genesis");
        assert_eq!(height, Height::GENESIS);
    }

    #[test]
    fn canonical_pushes_in_every_form_decode() {
        assert_eq!(height_of(&[0x51]).expect("OP_1"), 1);
        assert_eq!(height_of(&[0x60]).expect("OP_16"), 16);
        assert_eq!(height_of(&[0x01, 0x11]).expect("one digit"), 17);
        assert_eq!(height_of(&[0x02, 0x80, 0x00]).expect("sign byte"), 128);
        assert_eq!(
            height_of(&[0x03, 0x40, 0x42, 0x0f]).expect("three digits"),
            1_000_000
        );
        // 3,428,143 = 0x344f2f: three digits, top byte below 0x80, so no sign byte.
        assert_eq!(
            height_of(&[0x03, 0x2f, 0x4f, 0x34]).expect("three digits"),
            3_428_143
        );
        // 8,388,608 = 0x800000: the top digit would read as a sign, so a zero follows.
        assert_eq!(
            height_of(&[0x04, 0x00, 0x00, 0x80, 0x00]).expect("sign byte"),
            8_388_608
        );
        // Script data after the push is the coinbase's own business.
        assert_eq!(
            height_of(&[0x03, 0x40, 0x42, 0x0f, 0xaa, 0xbb]).expect("with data"),
            1_000_000
        );
    }

    #[test]
    fn non_canonical_pushes_are_rejected() {
        let rejected = |script: &[u8]| {
            assert!(
                matches!(height_of(script), Err(DecodeError::CoinbaseHeight(_))),
                "{script:02x?}"
            );
        };
        rejected(&[]);
        rejected(&[0x00]);
        rejected(&[0x01, 0x05]); // 5 must be OP_5
        rejected(&[0x02, 0x11, 0x00]); // 17 padded with a needless zero
        rejected(&[0x01, 0x80]); // 128 without its sign byte
        rejected(&[0x04, 0x2f, 0x4f, 0x34, 0x00]); // 3,428,143 with a needless sign byte
        rejected(&[0x03, 0x40, 0x42]); // truncated
        rejected(&[0x06, 1, 2, 3, 4, 5, 6]); // longer than any height
    }
}
