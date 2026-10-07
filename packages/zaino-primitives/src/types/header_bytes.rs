//! One consensus header's bytes (`getblockheader <h> false`, p2p `headers`, a block's prefix)
//!
//! - layout: version 4 · prev 32 · merkle 32 · commitments 32 · time 4 · bits 4 · nonce 32 ·
//!   compactsize solution (1344 = 200-9, 36 = regtest's 48-5)
//! - hash = SHA-256d of the bytes as received, never a field taken on trust

use super::{BlockCommitments, BlockHash, EquihashSolution, MerkleRoot};
use crate::sha256d;

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
        BlockHash::from(sha256d(self.bytes))
    }

    /// `int32` (zcashd `nVersion`): high bit set = negative
    pub fn version(&self) -> i32 {
        i32::from_le_bytes(self.array_at(0))
    }

    pub fn prev_hash(&self) -> BlockHash {
        BlockHash::from(self.array_at(4))
    }

    pub fn merkle_root(&self) -> MerkleRoot {
        MerkleRoot::from(self.array_at::<32>(36))
    }

    pub fn block_commitments(&self) -> BlockCommitments {
        BlockCommitments::from(self.array_at::<32>(68))
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
    use crate::testing::{encode_header, Chain};

    /// `encode_header` → `split` → every field back, the rest untouched; each layout fault named
    #[test]
    fn split_reads_back_every_encoded_field_and_names_each_layout_fault() {
        let chain = Chain::new();
        let header = chain.block(chain.genesis().hash).header().clone();
        let mut raw = encode_header(&header);
        let len = raw.len();
        raw.extend([7, 7]);

        let (split, rest) = HeaderBytes::split(&raw).expect("an encoded header");
        let fields = (
            (split.hash(), split.version(), split.prev_hash(), split.merkle_root()),
            (split.block_commitments(), split.time(), split.bits(), split.nonce()),
        );
        let expected = (
            (header.hash, header.version as i32, header.prev_hash, header.merkle_root),
            (header.block_commitments, header.time, header.bits.bits(), header.nonce),
        );
        assert_eq!(fields, expected);
        assert_eq!((split.as_bytes().len(), rest), (len, &[7u8, 7][..]));
        assert_eq!(split.equihash_solution(), header.solution);
        assert_eq!(split.equihash_input(), &raw[..EQUIHASH_INPUT]);

        let truncated = |len| Err(HeaderError::Truncated { len });
        assert_eq!(HeaderBytes::split(&raw[..NONCE_END]), truncated(NONCE_END));
        assert_eq!(HeaderBytes::split(&raw[..len - 1]), truncated(len - 1));
        let mut odd = raw[..=NONCE_END].to_vec();
        odd[NONCE_END] = 7;
        assert_eq!(HeaderBytes::split(&odd), Err(HeaderError::SolutionLength { len: 7 }));
        odd[NONCE_END] = 0xfe;
        assert_eq!(HeaderBytes::split(&odd), Err(HeaderError::SolutionPrefix { prefix: 0xfe }));
        let mut wide = raw[..NONCE_END].to_vec();
        wide.extend([0xfd, 36, 0]);
        wide.extend(&raw[NONCE_END + 1..len]);
        assert_eq!(HeaderBytes::split(&wide), Err(HeaderError::NonMinimalLength { len: 36 }));
    }
}
