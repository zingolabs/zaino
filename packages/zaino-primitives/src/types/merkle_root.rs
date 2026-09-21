//! Merkle root of the transaction tree.

/// Transaction merkle root (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MerkleRoot([u8; 32]);

impl From<[u8; 32]> for MerkleRoot {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<MerkleRoot> for [u8; 32] {
    fn from(m: MerkleRoot) -> Self {
        m.0
    }
}
