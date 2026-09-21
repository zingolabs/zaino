//! The non-finalised side of the seam, as the composer names it.

use zaino_core::{BlockHash, BlockId, CompactBlock, Height};

/// The minimal read the composer needs from a non-finalised chain-head to serve
/// compact blocks across the seam.
///
/// This is deliberately thinner than a fat read supertrait: the composer names
/// only what compact-block serving requires — a tip, a floor, and best-chain
/// compact-block lookup by height or hash. Other reads (transactions,
/// treestate, address history) are composed separately or passed through, so a
/// non-finalised head that only serves compact blocks need implement nothing
/// beyond this.
///
/// All lookups are over the **best chain** of the retained window `[floor,
/// tip]`. A height or hash on a side branch, or outside the window, is `None` —
/// absence, never an error; the window simply does not retain it.
pub trait NonFinalisedView: Send + Sync {
    /// Volatile best tip (height + hash), or `None` if the head holds nothing.
    fn tip(&self) -> Option<BlockId>;

    /// Lowest best-chain height retained in the window, if any. `Some` exactly
    /// when [`tip`](Self::tip) is `Some`.
    fn floor(&self) -> Option<Height>;

    /// Best-chain compact block at `height`, if the window retains it.
    fn compact_block_at(&self, height: Height) -> Option<CompactBlock>;

    /// Best-chain compact block by hash, if retained.
    fn compact_block_by_hash(&self, hash: BlockHash) -> Option<CompactBlock>;
}
