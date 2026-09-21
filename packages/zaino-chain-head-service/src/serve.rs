//! The non-finalised head as a composable serving segment.
//!
//! [`HeadSnapshot`] wraps a published [`MapBackedSnapshot`] and is both a
//! [`ChainSegment`] and a [`CompactBlockRead`]; a [`ChainHeadSubscriber`] is a
//! [`TakeSnapshot`] over it. Together these make the volatile head a segment a
//! composer stitches to the finalised store over one shared pin — exactly the
//! shape the store already provides, so the composer routes over both without
//! either side describing its own durability.
//!
//! The wrapper exists because the serving traits are foreign (they live in
//! `zaino-service`) and `Arc` is not a fundamental type, so the orphan rule
//! forbids implementing them directly for `Arc<MapBackedSnapshot>`. A local
//! newtype carries the impls while cloning as cheaply as the `Arc` it holds.
//!
//! All reads are in-memory over the retained window, so none can fail: a height
//! or hash the window does not hold — or a hash on a competing branch — is a
//! domain absence (`Ok(None)` / skipped), never a transient or fatal error.

use std::future::Future;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chain_head::{ChainHeadBlock, ChainHeadBlockService, ChainHeadSnapshot};
use zaino_core::{
    BlockId, BlockRef, ChainMetadata, CompactBlock, Height, HeightRange, PreIndexCompactBlock,
};
use zaino_service::error::{BlockReadError, ReadError, Transient};
use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};

use crate::snapshot::MapBackedSnapshot;
use crate::subscriber::ChainHeadSubscriber;

/// A published head window, served as a composable [`ChainSegment`].
///
/// A cheap-to-clone handle onto one published [`MapBackedSnapshot`]: cloning
/// clones the inner [`Arc`], not the graph. Immutable for the life of the pin,
/// so every read through one clone sees the same coherent window.
#[derive(Clone)]
pub struct HeadSnapshot(Arc<MapBackedSnapshot>);

/// Project a retained chain-head block onto its compact serving form.
///
/// Every per-block field comes from the block itself (via
/// [`PreIndexCompactBlock`]); the cumulative tree sizes come from the block's
/// own [`TreeRoots`](zaino_primitives::types::TreeRoots), which the head carries
/// per block — so the non-finalised side needs no cumulative index to serve
/// them.
fn to_compact(block: &ChainHeadBlock) -> CompactBlock {
    CompactBlock::from_pre_index(
        PreIndexCompactBlock::from(&block.block),
        ChainMetadata::from_tree_roots(&block.tree_roots),
    )
}

impl ChainSegment for HeadSnapshot {
    fn pinned_tip(&self) -> Option<BlockId> {
        // The head is never empty, so it always has a tip.
        let tip = self.0.best_tip();
        Some(BlockId {
            height: tip.height,
            hash: tip.hash,
        })
    }

    fn coverage(&self) -> Option<HeightRange> {
        // `[floor, tip]`: the lowest canonical height retained, up to the best
        // tip. The head is never empty, so this always covers at least the tip;
        // the floor falls back to the tip only in the degenerate single-block
        // window.
        let end = self.0.best_tip().height;
        let start = self
            .0
            .best_chain()
            .next()
            .map(ChainHeadBlock::height)
            .unwrap_or(end);
        Some(HeightRange { start, end })
    }
}

impl CompactBlockRead for HeadSnapshot {
    fn compact_block(
        &self,
        at: BlockRef,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send {
        // All lookups are over the best chain of the retained window; a
        // competing block or an out-of-window reference is a domain absence.
        let block = match at {
            BlockRef::Height(height) => self.0.best_block_by_height(height).map(to_compact),
            BlockRef::Hash(hash) => self
                .0
                .block_by_hash(&hash)
                .filter(|block| self.0.is_on_best_chain(block.reference))
                .map(to_compact),
        };
        std::future::ready(Ok(block))
    }

    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        // Eager inclusive iteration over the height span, mirroring the store's
        // `stream_compact`: yield each present best-chain block in height order,
        // skip an absent (e.g. above-tip) height as a domain absence.
        let start = u32::from(range.start);
        let end = u32::from(range.end);
        let blocks: Vec<Result<CompactBlock, ReadError>> = (start..=end)
            .filter_map(|height| Height::try_from(height).ok())
            .filter_map(|height| self.0.best_block_by_height(height).map(to_compact))
            .map(Ok)
            .collect();
        stream::iter(blocks).boxed()
    }
}

impl TakeSnapshot for ChainHeadSubscriber {
    type Snapshot = HeadSnapshot;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        // Capturing the current window is a cheap, infallible clone of an
        // already-published immutable snapshot — no I/O round-trip to fail.
        std::future::ready(Ok(HeadSnapshot(self.current())))
    }
}

#[cfg(test)]
mod tests {
    use super::ChainHeadSubscriber;
    use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};

    /// The head type-checks as a composer input: its snapshot is both a
    /// [`ChainSegment`] (coherence coordinate) and a [`CompactBlockRead`]
    /// (compact-block serving). Compile-time only — this is the bound the
    /// composer requires of each side of the seam.
    fn assert_bounds<T: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>>() {}

    #[test]
    fn head_is_a_valid_composer_input() {
        assert_bounds::<ChainHeadSubscriber>();
    }
}
