//! A block identified by both its hash and its height.

use crate::types::{BlockHash, Height};

/// A block named by both hash and height.
///
/// Used wherever a value must pin a specific block: a response echoing the
/// blocks it covered (so a caller can tell whether a reorg moved a range
/// underneath it), or a mempool set tagged with the validator tip it was read
/// against. A named pair rather than a tuple so `a.hash != b.hash` reads
/// correctly where `a.0 != b.0` invites the wrong field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    /// The block's hash.
    pub hash: BlockHash,
    /// The block's height.
    pub height: Height,
}

impl BlockRef {
    /// Build a reference from the `(hash, height)` pair a tip port answers with.
    pub fn from_tip(tip: (BlockHash, Height)) -> Self {
        let (hash, height) = tip;
        Self { hash, height }
    }
}
