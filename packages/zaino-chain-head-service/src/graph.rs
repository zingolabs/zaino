//! The contract every ChainHead graph representation upholds.
//!
//! [`ChainGraph`] is the mutable half of the graph — the moves the writer task
//! applies to build the next snapshot — sitting above the read-only
//! [`ChainHeadSnapshot`] a consumer sees. A representation implements both: the
//! read traits answer questions about a published view, and this trait is how
//! the runtime advances one.
//!
//! Keeping the moves behind a trait, rather than as inherent methods on one
//! map-backed type, is what lets the map-backed graph be swapped for a
//! persistent-structure one without touching the runtime: the runtime names
//! only these moves and only the read traits, never a concrete layout.

use zaino_chain_head::{snapshot::ChainHeadTransactionService, ChainHeadBlock, ChainHeadSnapshot};
use zaino_primitives::types::{BlockRef, Height};

#[cfg(test)]
pub(crate) mod tests;

/// The mutable moves a ChainHead graph supports, above its read capabilities.
///
/// # Invariants
///
/// Every implementation upholds these after every move, and a **refused move
/// leaves the graph unchanged**:
///
/// - the graph always holds its tip — it is never empty;
/// - every canonical block is retained;
/// - canonical heights run without gaps up to the tip;
/// - nothing above the tip is canonical;
/// - each canonical block's parent is the canonical block one height below.
pub(crate) trait ChainGraph:
    ChainHeadSnapshot + ChainHeadTransactionService + Clone
{
    /// A graph holding a single block, which becomes its tip and its only
    /// canonical block. Generation starts at zero.
    fn from_initial_block(block: ChainHeadBlock) -> Self;

    /// The canonical tip. Total, because the graph is never empty.
    fn tip_block(&self) -> &ChainHeadBlock;

    /// The retained block with the most accumulated work.
    ///
    /// On a tie the tip wins, so a graph already extended to the validator's
    /// answer is not pulled off it by an equal-work competitor. Total: with no
    /// heavier competitor this is the tip.
    fn heaviest_block(&self) -> &ChainHeadBlock;

    /// Appends a block as the new tip.
    ///
    /// Refused with [`NotChildOfTip`] unless `block` names the current tip as
    /// its parent and sits one height above it.
    fn extend(&mut self, block: ChainHeadBlock) -> Result<(), NotChildOfTip>;

    /// Makes a retained canonical block the tip, dropping the canonical status
    /// of everything above it.
    ///
    /// `best_chain' = { b ∈ best_chain | height(b) ≤ height(block) }`. The
    /// blocks above stay retained as a competing branch. A no-op when `block`
    /// is already the tip; refused with [`NotOnBestChain`] when `block` is not
    /// a retained canonical block.
    fn rewind_to(&mut self, block: BlockRef) -> Result<(), NotOnBestChain>;

    /// Drops retained blocks below `floor`, keeping the tip regardless.
    fn remove_finalized_blocks(&mut self, floor: Height);

    /// Stamps the generation this publication carries.
    ///
    /// A publication whose tip matches `previous` describes the same chain
    /// state and inherits its generation; one whose tip differs advances past
    /// `highest_published`. Taking `highest_published` rather than
    /// `previous.generation` is what keeps generations monotonic across a
    /// re-anchor, where the replacement graph starts from a single block at
    /// generation zero.
    fn stamp_generation(&mut self, previous: &Self, highest_published: u64);
}

/// A [`rewind_to`](ChainGraph::rewind_to) target that is not a retained
/// canonical block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block is not a retained canonical block")]
pub(crate) struct NotOnBestChain;

/// An [`extend`](ChainGraph::extend) block that does not sit directly above the
/// current tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "block {} at height {} does not extend the tip {} at height {}",
    block.hash,
    block.height,
    tip.hash,
    tip.height
)]
pub(crate) struct NotChildOfTip {
    /// The tip the block failed to extend.
    pub(crate) tip: BlockRef,
    /// The block that did not extend it.
    pub(crate) block: BlockRef,
}
