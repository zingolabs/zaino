//! Commitment tree state at a block.

use super::{BlockHash, BlockTime, Height};

/// Serialized commitment tree bytes for one pool.
///
/// An opaque blob in the Zcash protocol's own tree serialization — not JSON,
/// and not a Zaino shape. Interpreting it (which pool's node type, and
/// deserializing into a tree) is the consumer's business, so it crosses as
/// bytes rather than as a structure this crate would have to model.
pub type TreeBytes = Vec<u8>;

/// The commitment trees as of a block, and which block that is.
///
/// Carries its own block identity because a treestate is only meaningful
/// against one: the same bytes at a different height describe a different
/// chain. A caller that asked by height still needs the hash, and one that
/// asked by hash still needs the height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Treestate {
    /// Block these trees are the state after.
    pub block_hash: BlockHash,

    /// Height of that block.
    pub height: Height,

    /// Block time, in seconds since the Unix epoch.
    pub time: BlockTime,

    /// Serialized Sapling commitment tree, if the pool is active at this height.
    pub sapling: Option<TreeBytes>,

    /// Serialized Orchard commitment tree, if the pool is active at this height.
    pub orchard: Option<TreeBytes>,

    /// Serialized Ironwood commitment tree, if the pool is active at this
    /// height (NU6.3).
    pub ironwood: Option<TreeBytes>,
}
