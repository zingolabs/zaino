//! In-memory stand-in for the non-finalised side of the seam.
//!
//! [`StubNonFinalised`] holds a fixed set of best-chain compact blocks over
//! `[floor, tip]` and answers the shared `zaino-service` segment ports against
//! them. It stands in for the real `ChainGraph`-backed head, so the FS⊕NFS route
//! can be exercised end to end without wiring the volatile graph.
//!
//! A fixed window is already its own pinned view: it never advances, so
//! [`TakeSnapshot::snapshot`] just clones it. The real chain-head, which does
//! advance, captures a fresh snapshot per pin.

use std::collections::BTreeMap;
use std::future::Future;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{BlockHash, BlockId, BlockRef, ChainMetadata, CompactBlock, Height, HeightRange};
use zaino_primitives::types::CompactDifficulty;
use zaino_service::error::{BlockReadError, ReadError, Transient};
use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};

/// A fixed non-finalised window backed by an in-memory map.
///
/// Retains best-chain compact blocks keyed by height; the tip is the
/// highest-height block and the floor the lowest. Cheap to clone (the composer
/// clones the view into every snapshot).
#[derive(Clone, Debug, Default)]
pub struct StubNonFinalised {
    /// Best-chain compact blocks over `[floor, tip]`, keyed by height.
    blocks: BTreeMap<Height, CompactBlock>,
    /// The highest-height block's id, or `None` when empty.
    tip: Option<BlockId>,
}

impl StubNonFinalised {
    /// An empty window — the head holds nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A window over `blocks`. The floor and tip are derived from the contents
    /// (lowest and highest height); blocks whose height exceeds the protocol
    /// limit are dropped rather than retained.
    pub fn from_blocks(blocks: Vec<CompactBlock>) -> Self {
        let blocks: BTreeMap<Height, CompactBlock> = blocks
            .into_iter()
            .filter_map(|block| {
                Height::try_from(block.height)
                    .ok()
                    .map(|height| (height, block))
            })
            .collect();
        let tip = blocks.last_key_value().map(|(height, block)| BlockId {
            height: *height,
            hash: block.hash,
        });
        Self { blocks, tip }
    }
}

impl ChainSegment for StubNonFinalised {
    fn pinned_tip(&self) -> Option<BlockId> {
        self.tip
    }

    fn coverage(&self) -> Option<HeightRange> {
        let start = *self.blocks.keys().next()?;
        let end = *self.blocks.keys().next_back()?;
        Some(HeightRange { start, end })
    }
}

impl CompactBlockRead for StubNonFinalised {
    fn compact_block(
        &self,
        at: BlockRef,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send {
        // Best-effort over the stored window: a height maps directly, a hash
        // matches any stored block. Reads never fail in the stub.
        let block = match at {
            BlockRef::Height(height) => self.blocks.get(&height).cloned(),
            BlockRef::Hash(hash) => self
                .blocks
                .values()
                .find(|block| block.hash == hash)
                .cloned(),
        };
        std::future::ready(Ok(block))
    }

    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        let start = u32::from(range.start);
        let end = u32::from(range.end);
        let blocks: Vec<Result<CompactBlock, ReadError>> = (start..=end)
            .filter_map(|height| Height::try_from(height).ok())
            .filter_map(|height| self.blocks.get(&height).cloned())
            .map(Ok)
            .collect();
        stream::iter(blocks).boxed()
    }
}

impl TakeSnapshot for StubNonFinalised {
    type Snapshot = Self;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        std::future::ready(Ok(self.clone()))
    }
}

/// A filler compact block at `height` whose hash is `[hash_byte; 32]`.
///
/// Only the height and hash carry meaning for routing tests; the remaining
/// fields are inert placeholders.
pub fn stub_compact_block(height: u32, hash_byte: u8) -> CompactBlock {
    CompactBlock {
        hash: BlockHash::from([hash_byte; 32]),
        prev_hash: BlockHash::from([hash_byte.wrapping_sub(1); 32]),
        height,
        time: 0,
        bits: CompactDifficulty::try_from_bits(0x2007_ffff)
            .expect("0x2007ffff is a valid nBits encoding"),
        transactions: Vec::new(),
        chain_metadata: ChainMetadata::ZERO,
    }
}
