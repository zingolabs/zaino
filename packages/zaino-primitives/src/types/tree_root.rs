//! Commitment tree root hash.

/// A commitment tree root hash (32 bytes).
///
/// Distinct from [`super::BlockHash`] and [`super::TransactionHash`] —
/// same size, different domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeRoot([u8; 32]);

impl TreeRoot {
    /// Wrap raw root bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

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
