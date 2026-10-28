//! The moves that build a ChainHead graph.
//!
//! [`ChainHeadSnapshot`] is what a published graph answers. [`ChainGraph`] is
//! how the writer builds one. The service changes a graph only through these
//! moves, so any representation that implements them can back the chain head,
//! and one contract test suite covers every implementation.
//!
//! A branch switch is a rewind to the fork point followed by extensions. A
//! rollback is a rewind alone. Trimming removes old blocks and never the tip.

use zaino_chain_head::{snapshot::ChainHeadTransactionService, ChainHeadBlock, ChainHeadSnapshot};
use zaino_primitives::types::{BlockRef, Height};

#[cfg(test)]
pub(crate) mod tests;

/// A ChainHead graph under construction.
///
/// Every implementation keeps these invariants after every move:
///
/// - the graph holds its tip, so it is never empty;
/// - every canonical block is retained;
/// - the canonical heights run without gaps up to the tip, nothing above the
///   tip is canonical, and each canonical block's parent is the canonical block
///   one height below.
///
/// A refused move leaves the graph unchanged.
pub(crate) trait ChainGraph:
    ChainHeadSnapshot + ChainHeadTransactionService + Clone
{
    /// A graph that holds `block` alone, as its tip.
    fn from_initial_block(block: ChainHeadBlock) -> Self;

    /// The canonical tip block.
    fn tip_block(&self) -> &ChainHeadBlock;

    /// The retained block with the most accumulated work.
    ///
    /// Only strictly more work displaces the tip, so on a tie the tip is the
    /// answer. Among other blocks of equal work, any one may be returned.
    fn heaviest_block(&self) -> &ChainHeadBlock;

    /// Makes `block`, a child of the tip, the new tip.
    ///
    /// Refused unless `block` names the tip as its parent and sits one height
    /// above it. This is the only way a block becomes canonical. `block` may
    /// already be retained off the best chain.
    fn extend(&mut self, block: ChainHeadBlock) -> Result<(), NotChildOfTip>;

    /// Moves the tip down to `block`, which must already be canonical.
    ///
    /// `best_chain' = { b ∈ best_chain | height(b) ≤ height(block) }`
    ///
    /// Heights above `block` stop being canonical, and the blocks there stay
    /// retained as a competing branch. Rewinding to the tip changes nothing.
    /// Refused when `block` is not a retained canonical block: the fork point
    /// is further down.
    fn rewind_to(&mut self, block: BlockRef) -> Result<(), NotOnBestChain>;

    /// Drops every retained block below `floor`, on every branch, except the
    /// tip.
    ///
    /// When the tip is itself below `floor`, the graph holds the tip alone, and
    /// work still accumulates from it without reconnecting to the finalised
    /// state.
    fn remove_finalized_blocks(&mut self, floor: Height);

    /// Stamps the generation this publication carries.
    ///
    /// A publication whose tip matches `previous` describes the same chain
    /// state and inherits its generation. One whose tip differs advances past
    /// `highest_published`, the highest generation yet published. Taking that
    /// value rather than `previous`'s generation keeps generations monotonic
    /// across a re-anchor: a re-anchored graph starts at zero, so inheriting
    /// from the graph it replaces could repeat a generation, and a consumer
    /// holding the earlier epoch would be told its view was current.
    fn stamp_generation(&mut self, previous: &Self, highest_published: u64);
}

/// [`ChainGraph::rewind_to`] was asked for a block that is not a retained
/// canonical block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block is not a retained canonical block")]
pub(crate) struct NotOnBestChain;

/// [`ChainGraph::extend`] was given a block that is not a child of the tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "block {} at height {} does not extend the tip {} at height {}",
    block.hash,
    block.height,
    tip.hash,
    tip.height
)]
pub(crate) struct NotChildOfTip {
    /// The tip at the time of the refusal.
    pub(crate) tip: BlockRef,
    /// The refused block.
    pub(crate) block: BlockRef,
}
