//! A block's identity and position on the chain.

use zaino_primitives::types::{BlockHash, Height};

/// A block identified by hash and located by height.
///
/// The hash is the identity; the height is positional convenience. Two
/// blocks at the same height differ across a fork, so presence
/// predicates must key on the hash, never on the height alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId {
    /// Position on the chain the block belongs to.
    pub height: Height,
    /// The block's identity: its SHA-256d header hash.
    pub hash: BlockHash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_height_different_hash_are_distinct() {
        let height = Height::try_from(100).expect("valid height");
        let a = BlockId {
            height,
            hash: BlockHash::from([1u8; 32]),
        };
        let b = BlockId {
            height,
            hash: BlockHash::from([2u8; 32]),
        };
        assert_ne!(a, b);
    }
}
