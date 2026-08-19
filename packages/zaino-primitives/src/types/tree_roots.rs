//! Commitment tree roots and sizes.

use super::{TreeRoot, TreeSize};

/// Commitment tree roots and sizes at a specific block.
///
/// Every pool is `Option`: `None` means the block has no treestate for that
/// pool, either because it predates the pool's activation or because the
/// validator reported none. Consumers that need a defaulted root for an
/// inactive pool apply that default at their own boundary rather than having
/// it baked in here — an absent root and a zero root are different facts.
#[derive(Debug, Clone)]
pub struct TreeRoots {
    /// Sapling tree root and cumulative size, if pool is active.
    pub sapling: Option<TreeRootInfo>,
    /// Orchard tree root and cumulative size, if pool is active.
    pub orchard: Option<TreeRootInfo>,
    /// Ironwood tree root and cumulative size, if pool is active (NU6.3).
    pub ironwood: Option<TreeRootInfo>,
}

/// A tree root paired with its cumulative note count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRootInfo {
    /// The tree root hash.
    pub root: TreeRoot,
    /// Cumulative number of notes in the tree at this block.
    pub size: TreeSize,
}
