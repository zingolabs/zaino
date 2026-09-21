//! The block as ChainHead retains it, and the work that orders competing
//! branches.

use zaino_primitives::types::{Block, BlockHash, BlockRef, TreeRoots};

/// Cumulative proof-of-work measured from the ChainHead anchor, **not** from
/// genesis.
///
/// ChainHead never reads the finalised state, so it has no way to learn the
/// absolute chainwork of the block it anchors on. It does not need to: chain
/// selection is a comparison, and every branch retained in the window
/// accumulates from the same anchor past th reorg boundary, so the comparison
/// is exact even though the magnitudes are not absolute.
///
/// What this value is *not* is the `chainwork` a validator reports. Anything
/// that serves or persists absolute chainwork must rebase this against the
/// anchor's true cumulative work first.
///
/// The anchor is the parent of the window floor, named by
/// [`ChainHeadSnapshot::work_anchor`](crate::ChainHeadSnapshot::work_anchor).
/// Accumulation starts at the floor's *own* work rather than at zero, so the
/// value is always non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChainHeadWork(u128);

impl ChainHeadWork {
    /// The work contributed by a single block, as the base of a new
    /// accumulation. Used for the window floor, which has no retained parent.
    pub fn anchored_at(block_work: u128) -> Self {
        Self(block_work)
    }

    /// Extends this accumulation by one block's work.
    ///
    /// Returns `None` on overflow.
    pub fn checked_add(self, block_work: u128) -> Option<Self> {
        self.0.checked_add(block_work).map(Self)
    }

    /// The accumulated work as a plain integer.
    pub fn as_u128(self) -> u128 {
        self.0
    }
}

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
    /// Work accumulated from the ChainHead's anchor.
    pub work: ChainHeadWork,
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
