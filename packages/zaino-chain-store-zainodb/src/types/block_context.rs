//! Business-layer container pairing a [`BlockIndex`] with the block's
//! parent hash and cumulative chainwork.
//!
//! `BlockContext` deliberately has no serde impl — persistence is the sole
//! responsibility of a module-private helper in `types/db/block.rs`
//! (`PersistentBlockContext`), and the two types round-trip via `from_business`/
//! `to_business` conversion methods defined on that type.

use super::{AbsoluteChainWork, BlockHash, BlockIndex, Height};

/// The block's [`BlockIndex`], parent hash, and cumulative chainwork, where `Work` is [`AbsoluteChainWork`] once the total is known and `Option<AbsoluteChainWork>` where the producer may not know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockContext<Work = Option<AbsoluteChainWork>> {
    /// Uniquely identifies this block: its `(height, hash)` pair.
    pub index: BlockIndex,
    /// The hash of this block's parent block (previous block in chain).
    pub parent_hash: BlockHash,
    /// Total chain work up to this block, in whichever form `Work` names.
    pub chainwork: Work,
}

impl<Work> BlockContext<Work> {
    /// Constructs a new `BlockContext` by packaging `(height, hash)` into a
    /// [`BlockIndex`].
    pub fn new(hash: BlockHash, parent_hash: BlockHash, chainwork: Work, height: Height) -> Self {
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

    /// Returns the height of this block.
    pub fn height(&self) -> Height {
        self.index.height
    }

    /// The same context with its chainwork re-expressed by `map`.
    pub fn map_chainwork<Mapped>(self, map: impl FnOnce(Work) -> Mapped) -> BlockContext<Mapped> {
        BlockContext {
            index: self.index,
            parent_hash: self.parent_hash,
            chainwork: map(self.chainwork),
        }
    }
}

impl<Work: Copy> BlockContext<Work> {
    /// Returns the total chain work up to this block, in whichever form `Work` names.
    pub fn chainwork(&self) -> Work {
        self.chainwork
    }
}
