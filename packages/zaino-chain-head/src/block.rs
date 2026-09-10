//! The block as ChainHead retains it, and the work that orders competing
//! branches.

use zaino_primitives::types::{Block, BlockHash, BlockRef, RelativeChainWork, TreeRoots};

/// A block retained in the ChainHead window.
///
/// Carries the parsed block rather than its consensus bytes. That makes it a
/// projection: the fields an index reads, not everything the block hash
/// commits to. Serving a raw transaction or a raw block from ChainHead is
/// therefore not possible yet, and those queries stay on their existing path.
/// Storing the authoritative bytes alongside is the follow-up that closes it.
#[derive(Debug, Clone)]
pub struct ChainHeadBlock {
    /// This block's height and hash.
    pub reference: BlockRef,
    /// The parent block's hash. The graph's only edge.
    pub parent_hash: BlockHash,
    /// Work accumulated over the blocks this window retains above its anchor.
    ///
    /// Not the `chainwork` a validator reports. ChainHead never reads the
    /// finalised state, so the work below its anchor is unavailable to it and
    /// this total is measured from the anchor instead. Every branch in the
    /// window measures from that same anchor, which is what makes comparing
    /// these totals a correct choice between branches.
    pub work: RelativeChainWork,
    /// The parsed block.
    pub block: Block,
    /// Commitment tree roots and sizes after this block is applied.
    pub tree_roots: TreeRoots,
}

impl ChainHeadBlock {
    /// This block's hash.
    pub fn hash(&self) -> BlockHash {
        self.reference.hash
    }

    /// This block's height.
    pub fn height(&self) -> zaino_primitives::types::Height {
        self.reference.height
    }
}
