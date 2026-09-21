//! In-memory stand-in for the non-finalised side of the seam.
//!
//! [`StubNonFinalised`] holds a fixed set of best-chain compact blocks over
//! `[floor, tip]` and answers [`NonFinalisedView`] lookups against them. It
//! stands in for the real `ChainGraph`-backed head (a later stage), so the FS⊕
//! NFS route can be exercised end to end without wiring the volatile graph.

use std::collections::BTreeMap;

use zaino_core::{BlockHash, BlockId, ChainMetadata, CompactBlock, Height};
use zaino_primitives::types::CompactDifficulty;

use crate::view::NonFinalisedView;

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

impl NonFinalisedView for StubNonFinalised {
    fn tip(&self) -> Option<BlockId> {
        self.tip
    }

    fn floor(&self) -> Option<Height> {
        self.blocks.keys().next().copied()
    }

    fn compact_block_at(&self, height: Height) -> Option<CompactBlock> {
        self.blocks.get(&height).cloned()
    }

    fn compact_block_by_hash(&self, hash: BlockHash) -> Option<CompactBlock> {
        self.blocks
            .values()
            .find(|block| block.hash == hash)
            .cloned()
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
