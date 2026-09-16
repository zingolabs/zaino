//! The block as ChainHead retains it, and the work that orders competing
//! branches.

use zaino_primitives::types::{Block, BlockHash, BlockRef, ChainWork, TreeRoots};

/// Proof-of-work accumulated from the ChainHead anchor, **not** from genesis.
///
/// The anchor is the parent of the window floor: the block immediately below
/// the lowest one the graph retains. It contributes nothing itself, so for a
/// block `B` this value is the sum of block work over `(anchor, B]` and the
/// floor's own value is just the floor's block work.
///
/// # Why it is relative
///
/// ChainHead never reads the finalised state, so it cannot learn the anchor's
/// absolute chainwork. It does not need to: chain selection is a comparison,
/// and every branch retained in the window accumulates from that same anchor,
/// so the comparison is exact even though the magnitudes are not.
///
/// That uniformity is load-bearing. A graph holding some absolute values and
/// some relative ones has no usable ordering at all — any absolute value dwarfs
/// every relative one — so the heaviest branch would be chosen by which value
/// happened to be rebased rather than by work.
///
/// # Making it absolute
///
/// Because the anchor contributes zero, rebasing is one addition and no
/// subtraction:
///
/// ```text
/// absolute(B) = chainwork(anchor) + relative(B)
/// ```
///
/// [`ChainHeadSnapshot::work_anchor`](crate::ChainHeadSnapshot::work_anchor)
/// names the anchor so a consumer can look its chainwork up in a finalised
/// store; `zaino-chain` does exactly that, and answers `None` rather than a
/// guess until the store has built that far.
///
/// What this value is *not* is the `chainwork` a validator reports. That is why
/// it is its own type rather than [`zaino_primitives::types::ChainWork`]: the
/// two are not interchangeable and the type system should say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnchoredRelativeChainWork(ChainWork);

impl AnchoredRelativeChainWork {
    /// The anchor's own contribution: none.
    ///
    /// The identity this accumulation folds from, so the window floor is built
    /// by the same step as every block above it rather than by a special case.
    /// No *retained* block holds it — every one of them is at least its own
    /// block work above the anchor.
    pub const ZERO: Self = Self(ChainWork::ZERO);

    /// Extends this accumulation by one block's work.
    ///
    /// Returns `None` on overflow of the 256-bit total. The window spans a
    /// bounded number of blocks, so this cannot happen in practice; the caller
    /// still handles it rather than asserting, because "cannot happen" is a
    /// claim about the configuration, not about the type.
    pub fn checked_add(self, block_work: ChainWork) -> Option<Self> {
        self.0.checked_add(block_work.into()).map(Self)
    }

    /// The accumulated work as a chainwork magnitude.
    ///
    /// The same 256 bits consensus specifies, so a single block whose work
    /// exceeds a `u128` is representable here. Read the type's documentation
    /// before using it: this is measured from the anchor, not from genesis, and
    /// becomes absolute only by adding the anchor's own chainwork.
    pub fn as_chainwork(self) -> ChainWork {
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
    /// Work accumulated from the anchor. See
    /// [`AnchoredRelativeChainWork`] — this is not absolute chainwork, and
    /// becomes so only by adding the anchor's.
    pub work: AnchoredRelativeChainWork,
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
