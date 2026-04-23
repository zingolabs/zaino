//! Business-layer container pairing a [`BlockIndex`] with the block's
//! parent hash and cumulative chainwork.
//!
//! `BlockContext` deliberately has no serde impl — persistence is the sole
//! responsibility of a module-private helper in `types/db/block.rs`
//! (`PersistentBlockContext`), and the two types round-trip via `from_business`/
//! `to_business` conversion methods defined on that type.

use super::{BlockHash, BlockIndex, ChainWork, Height};

/// The block's [`BlockIndex`], parent hash, and cumulative chainwork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockContext {
    /// Uniquely identifies this block: its `(height, hash)` pair.
    pub index: BlockIndex,
    /// The hash of this block's parent block (previous block in chain).
    pub parent_hash: BlockHash,
    /// The cumulative proof-of-work of the blockchain up to this block,
    /// used for chain selection.
    pub chainwork: ChainWork,
}

impl BlockContext {
    /// Constructs a new `BlockContext` by packaging `(height, hash)` into a
    /// [`BlockIndex`].
    pub fn new(
        hash: BlockHash,
        parent_hash: BlockHash,
        chainwork: ChainWork,
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

    /// Returns the cumulative chainwork up to this block.
    pub fn chainwork(&self) -> &ChainWork {
        &self.chainwork
    }

    /// Returns the height of this block.
    pub fn height(&self) -> Height {
        self.index.height
    }
}
