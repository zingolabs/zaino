//! Business-layer container pairing a [`BlockIndex`] with the block's
//! parent hash and cumulative chainwork.
//!
//! `BlockContext` deliberately has no serde impl — persistence is the sole
//! responsibility of a module-private helper in `types/db/block.rs`
//! (`PersistentBlockContext`), and the two types round-trip via `from_business`/
//! `to_business` conversion methods defined on that type.

use super::{AbsoluteChainWork, BlockHash, BlockIndex, Height};

/// The block's [`BlockIndex`], parent hash, and cumulative chainwork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockContext {
    /// Uniquely identifies this block: its `(height, hash)` pair.
    pub index: BlockIndex,
    /// The hash of this block's parent block (previous block in chain).
    pub parent_hash: BlockHash,
    /// Total chain work up to this block, when it is known.
    ///
    /// `None` for a block the chain head produced: it holds a bounded window
    /// and never reads the finalised state, so the work below that window is
    /// not available to it. Such a block is served, never stored — persisting
    /// one is refused at the encode boundary.
    pub chainwork: Option<AbsoluteChainWork>,
}

impl BlockContext {
    /// Constructs a new `BlockContext` by packaging `(height, hash)` into a
    /// [`BlockIndex`].
    pub fn new(
        hash: BlockHash,
        parent_hash: BlockHash,
        chainwork: Option<AbsoluteChainWork>,
        height: Height,
    ) -> Self {
        Self {
            index: BlockIndex { height, hash },
            parent_hash,
            chainwork,
        }
    }

    /// Returns the hash of this block.
    pub fn hash(&self) -> &BlockHash {
        &self.index.hash
    }

    /// Returns the hash of the parent block.
    pub fn parent_hash(&self) -> &BlockHash {
        &self.parent_hash
    }

    /// Returns the total chain work up to this block, when it is known.
    pub fn chainwork(&self) -> Option<AbsoluteChainWork> {
        self.chainwork
    }

    /// Returns the height of this block.
    pub fn height(&self) -> Height {
        self.index.height
    }
}
