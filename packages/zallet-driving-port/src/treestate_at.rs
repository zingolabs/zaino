//! The note commitment tree state per shielded pool as of a block.

use crate::block_id::BlockId;
use crate::raw::RawTreeFrontier;

/// The note commitment tree state per shielded pool as of one block of
/// the pinned chain.
///
/// An absent pool frontier means an empty tree — the pool is not yet
/// active at this height, or holds no note commitments — never an
/// error (zcash/zallet#455). Frontiers cross the port as bytes per
/// ADR 0002; consumers own their parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreestateAt {
    /// The block this treestate is the state as of.
    pub at: BlockId,
    /// The Sapling note commitment tree frontier, absent for an empty
    /// tree.
    pub sapling: Option<RawTreeFrontier>,
    /// The Orchard note commitment tree frontier, absent for an empty
    /// tree.
    pub orchard: Option<RawTreeFrontier>,
    /// The Ironwood (NU6.3) note commitment tree frontier, absent for
    /// an empty tree.
    pub ironwood: Option<RawTreeFrontier>,
}
