//! Commitment tree root hash

/// Distinct from [`super::BlockHash`] & [`super::TransactionId`] (same size, different domain)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeRoot([u8; 32]);

impl From<[u8; 32]> for TreeRoot {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<TreeRoot> for [u8; 32] {
    fn from(r: TreeRoot) -> Self {
        r.0
    }
}
