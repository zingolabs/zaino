//! Block commitments field (hashBlockCommitments / hashFinalSaplingRoot).

/// Block commitments hash (32 bytes).
///
/// Pre-Sapling: `hashFinalSaplingRoot`. Post-Sapling: `hashBlockCommitments`
/// digest covering multiple commitment tree roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockCommitments([u8; 32]);

impl From<[u8; 32]> for BlockCommitments {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<BlockCommitments> for [u8; 32] {
    fn from(bc: BlockCommitments) -> Self {
        bc.0
    }
}
