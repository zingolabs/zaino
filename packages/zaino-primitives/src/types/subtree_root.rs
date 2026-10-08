//! Note commitment subtree root.

use super::{BlockHash, Height, TreeRoot};

/// A single subtree root entry from the commitment tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRoot {
    /// The root hash of this subtree.
    pub root: TreeRoot,
    /// The hash of the block that completed this subtree, in internal
    /// (unreversed) byte order. The lightwalletd wire carries it in display
    /// (reversed) order.
    pub completing_block_hash: BlockHash,
    /// The block height at which this subtree was completed.
    pub end_height: Height,
}
