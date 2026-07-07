//! Unified block iterator — walks `[start, end]` across both LMDB (frozen)
//! and in-memory (ChainStream) tiers transparently.

use std::sync::Arc;

use crate::chain_stream::ChainStream;
use crate::error::StoreError;
use crate::lmdb::LmdbStore;
use crate::types::{Block, Height};

/// A synchronous, owning iterator over blocks in a height range.
///
/// Created by [`crate::ChainState::stream_blocks`]. Handles the LMDB /
/// in-memory boundary internally — callers just iterate.
pub struct BlockIter {
    /// LMDB handle for frozen heights.
    lmdb: Arc<LmdbStore>,
    /// ChainStream for in-memory heights (initialised lazily when the LMDB
    /// phase completes).
    stream: Option<ChainStream>,
    /// Current cursor position.
    current: Height,
    /// Inclusive upper bound.
    end: Height,
    /// First height served by the in-memory stream (= chain.start at snap
    /// time). Heights below this come from LMDB.
    stream_start: Height,
}

impl BlockIter {
    pub(super) fn new(
        lmdb: Arc<LmdbStore>,
        stream: Option<ChainStream>,
        start: Height,
        end: Height,
        stream_start: Height,
    ) -> Self {
        Self {
            lmdb,
            stream,
            current: start,
            end,
            stream_start,
        }
    }

    /// Advance and return the next block.
    ///
    /// Returns `None` when the range is exhausted.
    /// Returns `Some(Err(...))` on store errors (caller decides whether to
    /// abort or skip).
    pub fn next(&mut self) -> Option<Result<Block, StoreError>> {
        if self.current > self.end {
            return None;
        }

        // Phase 1: below the in-memory chain — serve from LMDB.
        if self.current < self.stream_start {
            return Some(self.next_from_lmdb());
        }

        // Phase 2: in-memory via ChainStream.
        self.next_from_stream()
    }

    fn next_from_lmdb(&mut self) -> Result<Block, StoreError> {
        let h = self.current;
        self.current += 1;
        match self.lmdb.get(h)? {
            Some((_hash, block)) => Ok(block),
            None => Err(StoreError::HeightNotFound(h)),
        }
    }

    fn next_from_stream(&mut self) -> Option<Result<Block, StoreError>> {
        let stream = self.stream.as_mut()?;
        match stream.next() {
            Ok(Some(block)) => {
                self.current += 1;
                Some(Ok(block.clone()))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ChainState;
    use crate::types::{genesis_block, GENESIS_HASH};

    #[test]
    fn stream_blocks_in_memory_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let b1 = Block::new(1, [1u8; 32], GENESIS_HASH, vec![1]);
        let h1 = [1u8; 32];
        state.ingest(h1, b1).unwrap();

        let mut iter = state.stream_blocks(0, 1);
        let block0 = iter.next().unwrap().unwrap();
        assert_eq!(block0.height, 0);

        let block1 = iter.next().unwrap().unwrap();
        assert_eq!(block1.height, 1);

        assert!(iter.next().is_none());
    }

    #[test]
    fn stream_blocks_empty_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        let mut iter = state.stream_blocks(5, 4);
        assert!(iter.next().is_none());
    }
}
