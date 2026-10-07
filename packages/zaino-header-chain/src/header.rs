//! One consensus header from its raw bytes (`getblockheader <h> false`, p2p `headers`)
//!
//! - hash = SHA-256d of the bytes as received, never a field taken on trust
//! - layout: version 4 · prev 32 · merkle 32 · commitments 32 · time 4 · bits 4 · nonce 32 ·
//!   compactsize solution (1344 = 200-9, 36 = regtest's 48-5)

use sha2::{Digest, Sha256};
use zaino_primitives::types::{BlockHash, MerkleRoot};

/// Equihash's input: every byte before the nonce (zebra-chain `work/equihash.rs` `INPUT_LENGTH`)
pub(crate) const EQUIHASH_INPUT: usize = 108;
const NONCE_END: usize = EQUIHASH_INPUT + 32;
pub(crate) const STANDARD_SOLUTION: usize = 1344;
pub(crate) const REGTEST_SOLUTION: usize = 36;

/// A decoded header, its bytes kept for the Equihash check
#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    hash: BlockHash,
    bytes: Box<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("header truncated at {len} bytes")]
    Truncated { len: usize },
    #[error("solution of {len} bytes (200-9 = 1344, regtest 48-5 = 36)")]
    SolutionLength { len: usize },
    #[error("solution length prefix {prefix:#04x} (no header solution needs 2^32 bytes)")]
    SolutionPrefix { prefix: u8 },
    #[error("solution length {len} in three bytes (compactSize must be minimal)")]
    NonMinimalLength { len: usize },
    #[error("{trailing} bytes after the solution")]
    Trailing { trailing: usize },
}

/// Raw consensus bytes → [`Header`] (one exact header: nothing after the solution)
pub fn decode_header(raw: &[u8]) -> Result<Header, DecodeError> {
    let truncated = || DecodeError::Truncated { len: raw.len() };
    let (len, prefix) = match raw.get(NONCE_END).ok_or_else(truncated)? {
        0xfd => {
            let at = NONCE_END + 1;
            let bytes = raw.get(at..at + 2).ok_or_else(truncated)?;
            let len = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
            // zcashd `ReadCompactSize`: "non-canonical ReadCompactSize()"
            if len < 0xfd {
                return Err(DecodeError::NonMinimalLength { len });
            }
            (len, 3)
        }
        &small if small < 0xfd => (usize::from(small), 1),
        &prefix => return Err(DecodeError::SolutionPrefix { prefix }),
    };
    if len != STANDARD_SOLUTION && len != REGTEST_SOLUTION {
        return Err(DecodeError::SolutionLength { len });
    }
    let end = NONCE_END + prefix + len;
    match raw.len().cmp(&end) {
        std::cmp::Ordering::Less => Err(truncated()),
        std::cmp::Ordering::Greater => Err(DecodeError::Trailing { trailing: raw.len() - end }),
        std::cmp::Ordering::Equal => {
            let hash: [u8; 32] = Sha256::digest(Sha256::digest(raw)).into();
            Ok(Header { hash: BlockHash::from(hash), bytes: raw.into() })
        }
    }
}

impl Header {
    pub fn hash(&self) -> BlockHash {
        self.hash
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

    pub fn time(&self) -> u32 {
        self.u32_at(100)
    }

    pub fn bits(&self) -> u32 {
        self.u32_at(104)
    }

    /// Consensus bytes as received
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn equihash_input(&self) -> &[u8] {
        &self.bytes[..EQUIHASH_INPUT]
    }

    pub(crate) fn nonce(&self) -> &[u8] {
        &self.bytes[EQUIHASH_INPUT..NONCE_END]
    }

    /// After its compactsize prefix (decode admitted only the two lengths)
    pub(crate) fn solution(&self) -> &[u8] {
        let len = match self.bytes.len() - NONCE_END {
            standard if standard == STANDARD_SOLUTION + 3 => STANDARD_SOLUTION,
            _ => REGTEST_SOLUTION,
        };
        &self.bytes[self.bytes.len() - len..]
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.array_at(at))
    }

    fn array_at<const N: usize>(&self, at: usize) -> [u8; N] {
        self.bytes[at..at + N].try_into().expect("decode checked the fixed fields")
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Header")
            .field("hash", &self.hash)
            .field("prev_hash", &self.prev_hash())
            .field("time", &self.time())
            .field("bits", &format_args!("{:#010x}", self.bits()))
            .finish()
    }
}
