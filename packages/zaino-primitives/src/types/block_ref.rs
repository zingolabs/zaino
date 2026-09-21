//! A block named by both hash and height.

use crate::types::{BlockHash, Height};

/// A block named by both hash and height.
///
/// Used wherever a value has to identify *which* block it was computed against:
/// a response echoing back the range it covered, so the caller can tell whether
/// a reorg has moved that range underneath it, or a mempool set tagged with the
/// tip it was read at, so a later reader can judge whether the set is still
/// coherent with the chain.
///
/// A named pair rather than a tuple because the comparisons that matter are
/// per-field — `tip.hash != other.hash` reads correctly where `tip.0 != other.0`
/// invites the wrong field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    /// The block's hash.
    pub hash: BlockHash,
    /// The block's height.
    pub height: Height,
}

impl BlockRef {
    /// Build a reference from the pair the tip ports answer with.
    pub fn from_tip(tip: (BlockHash, Height)) -> Self {
        let (hash, height) = tip;
        Self { hash, height }
    }
}

impl From<(BlockHash, Height)> for BlockRef {
    fn from(tip: (BlockHash, Height)) -> Self {
        Self::from_tip(tip)
    }
}
