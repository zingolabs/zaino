//! One consensus header's bytes (`getblockheader <h> false`, p2p `headers`, a block's prefix)
//!
//! - layout: version 4 · prev 32 · merkle 32 · commitments 32 · time 4 · bits 4 · nonce 32 ·
//!   compactsize solution (1344 = 200-9, 36 = regtest's 48-5)
//! - hash = SHA-256d of the bytes as received, never a field taken on trust

use super::{BlockCommitments, EquihashSolution, MerkleRoot};
use crate::sha256d;
use zcash_protocol::BlockHash;

/// Equihash's input: every byte before the nonce (zebra-chain `work/equihash.rs` `INPUT_LENGTH`)
const EQUIHASH_INPUT: usize = 108;
const NONCE_END: usize = EQUIHASH_INPUT + 32;
pub const STANDARD_SOLUTION: usize = 1344;
pub const REGTEST_SOLUTION: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("header truncated at {len} bytes")]
    Truncated { len: usize },
    #[error("solution of {len} bytes (200-9 = 1344, regtest 48-5 = 36)")]
    SolutionLength { len: usize },
    #[error("solution length prefix {prefix:#04x} (no header solution needs 2^32 bytes)")]
    SolutionPrefix { prefix: u8 },
    #[error("solution length {len} in three bytes (compactSize must be minimal)")]
    NonMinimalLength { len: usize },
}

/// Layout checked: fixed fields present, solution 1344 or 36 bytes behind a minimal compactsize
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderBytes<'a> {
    bytes: &'a [u8],
    solution_at: usize,
}

impl<'a> HeaderBytes<'a> {
    /// Header opening `raw` + the bytes after it
    pub fn split(raw: &'a [u8]) -> Result<(Self, &'a [u8]), HeaderError> {
        let truncated = || HeaderError::Truncated { len: raw.len() };
        let (len, prefix) = match raw.get(NONCE_END).ok_or_else(truncated)? {
            0xfd => {
                let at = NONCE_END + 1;
                let bytes = raw.get(at..at + 2).ok_or_else(truncated)?;
                let len = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
                // zcashd `ReadCompactSize`: "non-canonical ReadCompactSize()"
                if len < 0xfd {
                    return Err(HeaderError::NonMinimalLength { len });
                }
                (len, 3)
            }
            &small if small < 0xfd => (usize::from(small), 1),
            &prefix => return Err(HeaderError::SolutionPrefix { prefix }),
        };
        if len != STANDARD_SOLUTION && len != REGTEST_SOLUTION {
            return Err(HeaderError::SolutionLength { len });
        }
        let end = NONCE_END + prefix + len;
        if raw.len() < end {
            return Err(truncated());
        }
        let (bytes, rest) = raw.split_at(end);
        Ok((Self { bytes, solution_at: NONCE_END + prefix }, rest))
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn hash(&self) -> BlockHash {
        BlockHash(sha256d(self.bytes))
    }

    /// `int32` (zcashd `nVersion`): high bit set = negative
    pub fn version(&self) -> i32 {
        i32::from_le_bytes(self.array_at(0))
    }

    pub fn prev_hash(&self) -> BlockHash {
        BlockHash(self.array_at(4))
    }

    pub fn merkle_root(&self) -> MerkleRoot {
        MerkleRoot(self.array_at::<32>(36))
    }

    pub fn block_commitments(&self) -> BlockCommitments {
        BlockCommitments(self.array_at::<32>(68))
    }

    pub fn time(&self) -> u32 {
        u32::from_le_bytes(self.array_at(100))
    }

    pub fn bits(&self) -> u32 {
        u32::from_le_bytes(self.array_at(104))
    }

    pub fn equihash_input(&self) -> &'a [u8] {
        &self.bytes[..EQUIHASH_INPUT]
    }

    pub fn nonce(&self) -> [u8; 32] {
        self.array_at(EQUIHASH_INPUT)
    }

    /// After its compactsize prefix
    pub fn solution(&self) -> &'a [u8] {
        &self.bytes[self.solution_at..]
    }

    pub fn equihash_solution(&self) -> EquihashSolution {
        let solution = self.solution();
        match <[u8; STANDARD_SOLUTION]>::try_from(solution) {
            Ok(standard) => EquihashSolution::Standard(standard),
            Err(_) => EquihashSolution::Regtest(
                solution.try_into().expect("split admits only 1344- or 36-byte solutions"),
            ),
        }
    }

    fn array_at<const N: usize>(&self, at: usize) -> [u8; N] {
        self.bytes[at..at + N].try_into().expect("split checked the fixed fields")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header laid out field by field, two trailing bytes → `split` → every field back, the rest
    /// untouched
    #[test]
    fn split_reads_back_every_field_and_leaves_the_bytes_after_the_header() {
        let mut raw = Vec::new();
        raw.extend(4i32.to_le_bytes()); // version
        raw.extend([0x11; 32]); // prev_hash
        raw.extend([0x22; 32]); // merkle_root
        raw.extend([0x33; 32]); // block_commitments
        raw.extend(1_700_000_000u32.to_le_bytes()); // time
        raw.extend(0x200f_0f0fu32.to_le_bytes()); // bits
        raw.extend([0x44; 32]); // nonce
        raw.push(36); // solution length
        raw.extend([0x55; 36]); // solution
        let header_len = raw.len();
        raw.extend([7, 7]);

        let (split, rest) = HeaderBytes::split(&raw).expect("a laid-out header");

        assert_eq!(split.hash(), BlockHash(sha256d(&raw[..header_len])), "hash");
        assert_eq!(split.version(), 4, "version");
        assert_eq!(split.prev_hash(), BlockHash([0x11; 32]), "prev_hash");
        assert_eq!(split.merkle_root(), MerkleRoot([0x22; 32]), "merkle_root");
        assert_eq!(split.block_commitments(), BlockCommitments([0x33; 32]), "block_commitments");
        assert_eq!(split.time(), 1_700_000_000, "time");
        assert_eq!(split.bits(), 0x200f_0f0f, "bits");
        assert_eq!(split.nonce(), [0x44; 32], "nonce");
        assert_eq!(split.equihash_solution(), EquihashSolution::Regtest([0x55; 36]), "solution");
        assert_eq!(split.equihash_input(), &raw[..EQUIHASH_INPUT], "equihash input");
        assert_eq!(split.as_bytes().len(), header_len, "header bytes");
        assert_eq!(rest, [7, 7], "bytes after the header");
    }

    #[test]
    fn split_names_each_layout_fault() {
        let fixed = [0u8; NONCE_END];
        let header = |solution_prefix: &[u8], solution: usize| {
            [&fixed[..], solution_prefix, &vec![0x55; solution]].concat()
        };
        let regtest = header(&[36], 36);
        let short = regtest.len() - 1;
        use HeaderError::*;
        #[rustfmt::skip]
        let cases = [
            ("no solution length",      fixed.to_vec(),              Truncated { len: NONCE_END }),
            ("solution one byte short", regtest[..short].to_vec(),   Truncated { len: short }),
            ("7-byte solution",         header(&[7], 7),             SolutionLength { len: 7 }),
            ("0xfe length prefix",      header(&[0xfe], 0),          SolutionPrefix { prefix: 0xfe }),
            ("36 in three bytes",       header(&[0xfd, 36, 0], 36),  NonMinimalLength { len: 36 }),
        ];
        for (case, raw, expected) in cases {
            assert_eq!(HeaderBytes::split(&raw), Err(expected), "{case}");
        }
    }
}
