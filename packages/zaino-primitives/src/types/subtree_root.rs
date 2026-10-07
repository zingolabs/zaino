//! Note commitment subtree root

use super::{BlockRef, TreeRoot};

/// Completed 2^16-leaf subtree's root + the block whose commitments completed it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRoot {
    pub root: TreeRoot,
    pub completing: BlockRef,
}
