//! Commitment trees as of a block

use super::{BlockHash, BlockTime, Height};

/// Every pool always present
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Treestate {
    pub block_hash: BlockHash,
    pub height: Height,
    pub time: BlockTime,
    pub sapling: CommitmentTreeBytes,
    pub orchard: CommitmentTreeBytes,
    pub ironwood: CommitmentTreeBytes,
}

/// One pool's `write_commitment_tree` bytes (`000000` = active and empty)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitmentTreeBytes(Vec<u8>);

impl CommitmentTreeBytes {
    /// Panics on empty (an absent tree reads as the empty tree post-activation: silently wrong)
    pub fn new(serialized: Vec<u8>) -> Self {
        assert!(!serialized.is_empty(), "a commitment tree serializes to >= 3 bytes");
        Self(serialized)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
