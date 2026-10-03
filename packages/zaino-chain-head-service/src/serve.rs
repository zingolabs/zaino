//! The non-finalised head as a composable serving segment.
//!
//! [`HeadSnapshot`] wraps a published [`MapBackedSnapshot`] and is a
//! [`ChainSegment`], a [`CompactBlockRead`] and a [`HeaderRead`]; a
//! [`ChainHeadSubscriber`] is a [`TakeSnapshot`] over it. Together these make the
//! volatile head a segment a composer stitches to the finalised store over one
//! shared pin — exactly the shape the store already provides, so the composer
//! routes over both without either side describing its own durability.
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
use zaino_primitives::types::{
    BlockRef, BlockSelector, ChainMetadata, CompactBlock, Height, HeightRange, PreIndexCompactBlock,
};
use zaino_service::error::{BlockReadError, ReadError, Transient};
use zaino_service::{ChainSegment, CompactBlockRead, HeaderRead, HeaderSummary, TakeSnapshot};

use crate::snapshot::MapBackedSnapshot;
use crate::subscriber::ChainHeadSubscriber;

/// A published head window, served as a composable [`ChainSegment`].
///
/// A cheap-to-clone handle onto one published [`MapBackedSnapshot`]: cloning
/// clones the inner [`Arc`], not the graph. Immutable for the life of the pin,
/// so every read through one clone sees the same coherent window.
#[derive(Clone)]
pub struct HeadSnapshot(Arc<MapBackedSnapshot>);

impl HeadSnapshot {
    /// The retained window this view is pinned to, for the reads that live in
    /// sibling modules.
    pub(crate) fn window(&self) -> &MapBackedSnapshot {
        &self.0
    }
}

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
    fn pinned_tip(&self) -> Option<BlockRef> {
        // The head is never empty, so it always has a tip.
        let tip = self.0.best_tip();
        Some(BlockRef {
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
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send {
        // All lookups are over the best chain of the retained window; a
        // competing block or an out-of-window reference is a domain absence.
        let block = match at {
            BlockSelector::Height(height) => self.0.best_block_by_height(height).map(to_compact),
            BlockSelector::Hash(hash) => self
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

impl HeaderRead for HeadSnapshot {
    fn header(
        &self,
        h: Height,
    ) -> impl Future<Output = Result<Option<HeaderSummary>, BlockReadError>> + Send {
        // The header projection of the best-chain block at `h`; an out-of-window
        // or off-best-chain height is a domain absence. In-memory, so it never
        // fails.
        let summary = self.0.best_block_by_height(h).map(|block| HeaderSummary {
            hash: block.reference.hash,
            time: block.block.header.time,
        });
        std::future::ready(Ok(summary))
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
    use std::sync::Arc;

    use super::{ChainHeadSubscriber, HeadSnapshot};
    use crate::graph::ChainGraph;
    use crate::snapshot::MapBackedSnapshot;
    use zaino_chain_head::{ChainHeadBlock, ChainHeadWork};
    use zaino_primitives::types::{
        Block, BlockCommitments, BlockHash, BlockHeader, BlockRef, BlockTime, ChainMetadata,
        CompactDifficulty, EquihashSolution, Height, MerkleRoot, TreeRoots,
    };
    use zaino_service::{ChainSegment, CompactBlockRead, HeaderRead, TakeSnapshot};

    /// The head type-checks as a composer input: its snapshot is a
    /// [`ChainSegment`] (coherence coordinate), a [`CompactBlockRead`]
    /// (compact-block serving) and a [`HeaderRead`] (the header projection).
    /// Compile-time only — this is the bound the composer requires of each side
    /// of the seam.
    fn assert_bounds<T: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead + HeaderRead>>() {}

    #[test]
    fn head_is_a_valid_composer_input() {
        assert_bounds::<ChainHeadSubscriber>();
    }

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// A chain-head block at `height` with hash `[hash_byte; 32]`, parent
    /// `[parent_byte; 32]`, and the given timestamp.
    fn head_block(h: u32, hash_byte: u8, parent_byte: u8, time: BlockTime) -> ChainHeadBlock {
        let hash = BlockHash::from([hash_byte; 32]);
        let parent_hash = BlockHash::from([parent_byte; 32]);
        ChainHeadBlock {
            reference: BlockRef {
                hash,
                height: height(h),
            },
            parent_hash,
            work: ChainHeadWork::anchored_at(u128::from(h)),
            block: Block {
                header: BlockHeader {
                    hash,
                    version: 4,
                    prev_hash: parent_hash,
                    height: height(h),
                    time,
                    merkle_root: MerkleRoot::from([0; 32]),
                    block_commitments: BlockCommitments::from([0; 32]),
                    bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                    nonce: [0; 32],
                    solution: EquihashSolution::Regtest([0; 36]),
                },
                transactions: vec![],
                chain_metadata: ChainMetadata::ZERO,
            },
            tree_roots: TreeRoots {
                sapling: None,
                orchard: None,
                ironwood: None,
            },
        }
    }

    #[tokio::test]
    async fn header_reads_an_in_window_block() {
        let mut graph = MapBackedSnapshot::from_initial_block(head_block(0, 0, 0xFF, 1000));
        graph
            .extend(head_block(1, 1, 0, 1001))
            .expect("height 1 extends the tip");
        let head = HeadSnapshot(Arc::new(graph));

        let summary = head
            .header(height(1))
            .await
            .expect("read succeeds")
            .expect("height 1 is in the window");
        assert_eq!(summary.hash, BlockHash::from([1u8; 32]));
        assert_eq!(summary.time, 1001);
    }

    #[tokio::test]
    async fn header_above_the_window_is_a_domain_miss() {
        let head = HeadSnapshot(Arc::new(MapBackedSnapshot::from_initial_block(head_block(
            0, 0, 0xFF, 1000,
        ))));
        assert_eq!(head.header(height(9)).await.expect("read succeeds"), None);
    }
}
