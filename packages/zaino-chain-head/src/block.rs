//! The block as ChainHead retains it, and the work that orders competing
//! branches.

use zaino_primitives::types::{Block, BlockHash, BlockRef, TreeRoots};

/// Cumulative proof-of-work measured from the ChainHead anchor, **not** from
/// genesis.
///
/// ChainHead never reads the finalised state, so it has no way to learn the
/// absolute chainwork of the block it anchors on. It does not need to: chain
/// selection is a comparison, and every branch retained in the window
/// accumulates from that same anchor, so the comparison is exact even though
/// the magnitudes are not absolute.
///
/// What this value is *not* is the `chainwork` a validator reports. Anything
/// that serves or persists absolute chainwork must rebase this against the
/// anchor's true cumulative work first. That rebasing is not implemented
/// anywhere yet — which is why this is its own type rather than
/// [`zaino_primitives::types::ChainWork`]: the two are not interchangeable and
/// the type system should say so.
///
/// Accumulation starts at the anchor block's *own* work rather than at zero,
/// so the value is always non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChainHeadWork(u128);

impl ChainHeadWork {
    /// The work contributed by a single block, as the base of a new
    /// accumulation. Used for the anchor, which has no retained parent.
    pub fn anchored_at(block_work: u128) -> Self {
        Self(block_work)
    }

    /// Extends this accumulation by one block's work.
    ///
    /// Returns `None` on overflow. The window spans a bounded number of blocks,
    /// so this cannot happen in practice; the caller still handles it rather
    /// than asserting, because "cannot happen" is a claim about the
    /// configuration, not about the type.
    pub fn checked_add(self, block_work: u128) -> Option<Self> {
        self.0.checked_add(block_work).map(Self)
    }

    /// The accumulated work as a plain integer.
    ///
    /// For consumers that must hand this to something expecting a chainwork
    /// magnitude. Read the type's documentation first: this is anchor-relative.
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
    /// Work accumulated from the anchor. See [`ChainHeadWork`] — this is not
    /// absolute chainwork.
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
