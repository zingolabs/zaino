//! Commitment tree state at a block.

use super::{BlockHash, BlockTime, Height, TreeRoot};

/// Serialized commitment tree bytes for one pool.
///
/// An opaque blob in the Zcash protocol's own tree serialization — not JSON,
/// and not a Zaino shape. Interpreting it (which pool's node type, and
/// deserializing into a tree) is the consumer's business, so it crosses as
/// bytes rather than as a structure this crate would have to model.
pub type TreeBytes = Vec<u8>;

/// One pool's commitment tree at a block.
///
/// The tree and its root are kept together because a root without its tree is
/// not a treestate, and the interface reports them as one object per pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTreestate {
    /// Root of the pool's note commitment tree after this block.
    ///
    /// `None` when the answering source does not report one. Zebra does not —
    /// its own response type documents the field as unused — so this is
    /// genuinely absent rather than zeroed, and a consumer must render it as an
    /// absent field rather than inventing a value.
    ///
    /// Held in internal byte order, like every other identifier in this crate.
    /// `z_gettreestate` writes it in display order, so the reversal belongs at
    /// the wire boundary.
    pub final_root: Option<TreeRoot>,

    /// The pool's serialized note commitment tree.
    pub final_state: TreeBytes,
}

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

    /// Sapling commitment tree, if the pool is active at this height.
    ///
    /// `None` is the pool having no tree at this block, not an empty tree:
    /// `z_gettreestate` keys on absence to omit pre-activation pools, so
    /// reporting a serialized empty tree would claim the pool is active.
    pub sapling: Option<PoolTreestate>,

    /// Orchard commitment tree, if the pool is active at this height.
    pub orchard: Option<PoolTreestate>,

    /// Ironwood commitment tree, if the pool is active at this height (NU6.3).
    pub ironwood: Option<PoolTreestate>,
}

impl Treestate {
    /// The commitment trees alone, without their roots.
    pub fn into_trees(self) -> CommitmentTrees {
        let tree = |pool: Option<PoolTreestate>| pool.map(|pool| pool.final_state);
        CommitmentTrees {
            block_hash: self.block_hash,
            height: self.height,
            time: self.time,
            sapling: tree(self.sapling),
            orchard: tree(self.orchard),
            ironwood: tree(self.ironwood),
        }
    }
}

/// The commitment trees as of a block, without their roots.
///
/// What a light wallet builds witnesses from: it parses each tree itself and
/// never reads a root. A root is a function of its tree, but computing one hashes
/// up the whole tree depth (milliseconds per pool), so this shape exists for the
/// reads that do not need it. [`Treestate`] is the shape that carries roots.
///
/// The pool fields keep [`Treestate`]'s meaning: `None` is the pool having no
/// tree at this block (below its activation), never an empty tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitmentTrees {
    /// Block these trees are the state after.
    pub block_hash: BlockHash,

    /// Height of that block.
    pub height: Height,

    /// Block time, in seconds since the Unix epoch.
    pub time: BlockTime,

    /// Sapling commitment tree, if the pool is active at this height.
    pub sapling: Option<TreeBytes>,

    /// Orchard commitment tree, if the pool is active at this height.
    pub orchard: Option<TreeBytes>,

    /// Ironwood commitment tree, if the pool is active at this height (NU6.3).
    pub ironwood: Option<TreeBytes>,
}
