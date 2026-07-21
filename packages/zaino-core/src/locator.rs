//! Fork-reconciliation vocabulary.

use zaino_primitives::types::{BlockHash, Height};

/// A descending-by-height sample of block hashes, offered to locate the fork
/// point between a driver's view and the pinned best chain.
#[derive(Clone, Debug)]
pub struct Locator(pub Vec<BlockHash>);

/// The highest locator entry that sits on the pinned chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForkPoint {
    pub height: Height,
    pub hash: BlockHash,
}
