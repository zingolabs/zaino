//! Header commitments field

/// Pre-Sapling `hashFinalSaplingRoot`; post `hashBlockCommitments` (over several tree roots)
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
