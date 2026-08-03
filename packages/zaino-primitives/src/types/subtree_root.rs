//! Note commitment subtree root.

use super::{Height, TreeRoot};

/// A single subtree root entry from the commitment tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRoot {
    /// The root hash of this subtree.
    pub root: TreeRoot,
    /// The block height at which this subtree was completed.
    pub end_height: Height,
}
