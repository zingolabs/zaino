//! Commitment tree root hash

/// Distinct from a block hash & a txid (same size, different domain)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeRoot(pub [u8; 32]);
