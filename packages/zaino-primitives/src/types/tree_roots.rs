//! Commitment tree roots and sizes.

use super::{TreeRoot, TreeSize};

/// Commitment tree roots and sizes at a specific block.
#[derive(Debug, Clone)]
pub struct TreeRoots {
    /// Sapling tree root and cumulative size, if pool is active.
    pub sapling: Option<TreeRootInfo>,
    /// Orchard tree root and cumulative size, if pool is active.
    pub orchard: Option<TreeRootInfo>,
}

/// A tree root paired with its cumulative note count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRootInfo {
    /// The tree root hash.
    pub root: TreeRoot,
    /// Cumulative number of notes in the tree at this block.
    pub size: TreeSize,
}
