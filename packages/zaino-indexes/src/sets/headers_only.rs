//! Headers-only index set: minimal context, one index.
//!
//! Suitable for benchmarking header sync throughput. The set-wide
//! context carries only header fields.

use zaino_primitives::types::{Block, BlockHash, BlockTime, CompactDifficulty};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::BlockHeight;
use zaino_sync::traits::ProvideContext;

use crate::indexes::headers::{HeaderCtx, HeadersIndex};

/// Set-wide context for the headers-only index set.
#[derive(Debug, Clone)]
pub struct HeadersOnlyContext {
    /// Block height (sync-engine type).
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty (nBits).
    pub bits: CompactDifficulty,
}

/// Build a [`HeadersOnlyContext`] from a domain [`Block`].
pub fn context_from_block(block: &Block) -> HeadersOnlyContext {
    HeadersOnlyContext {
        height: BlockHeight::new(u64::from(block.header.height)),
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        time: block.header.time,
        bits: block.header.bits,
    }
}

impl ProvideContext<HeaderCtx> for HeadersOnlyContext {
    fn context(&self) -> HeaderCtx {
        HeaderCtx {
            height: self.height,
            hash: self.hash,
            prev_hash: self.prev_hash,
            time: self.time,
            bits: self.bits,
        }
    }
}

/// Build the headers-only index set.
pub fn index_set() -> IndexSet<HeadersOnlyContext> {
    IndexSet::new().with::<HeadersIndex>()
}
