/// Pre-Sapling `hashFinalSaplingRoot`, post `hashBlockCommitments` (over several tree roots)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockCommitments(pub [u8; 32]);
