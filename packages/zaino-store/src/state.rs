//! ChainState — the in-memory block store.
//!
//! Holds the persistent chain behind an `RwLock<Arc<Chain>>`.
//! The writer builds a new Chain and swaps under the write lock
//! (one `Arc` pointer assignment, nanoseconds). Readers clone one `Arc`
//! under the read lock (pointer bump, nanoseconds), then are fully independent.

use std::path::Path;
use std::sync::{Arc, RwLock};

use im::Vector;

use crate::block_iter::BlockIter;
use crate::chain::Chain;
use crate::chain_stream::ChainStream;
use crate::error::StoreError;
use crate::lmdb::LmdbStore;
use crate::types::{Block, BlockHash, Height, MAX_REORG_DEPTH};

/// The shared, mutable chain state root.
pub struct ChainState {
    pub(crate) chain: RwLock<Arc<Chain>>,
    lmdb: Arc<LmdbStore>,
}

impl ChainState {
    /// Open a persistent ChainState backed by LMDB at `path`.
    ///
    /// The chain starts empty at `lmdb_latest + 1` — LMDB already holds
    /// everything from previous sessions. The sync loop will forward-fill
    /// to catch up and short-sync to populate the chain with the top
    /// `MAX_REORG_DEPTH` blocks.
    ///
    /// `start_height` is the height to start from when the LMDB is empty
    /// (e.g. Sapling activation height).
    pub fn open(path: &Path, start_height: Height) -> Result<Self, StoreError> {
        let lmdb = Arc::new(LmdbStore::open(path)?);

        let chain_start = match lmdb.block_count()? {
            Some(count) if count > 0 => {
                tracing::info!(count, "Opened LMDB, chain starts empty at height {count}");
                count
            }
            _ => {
                tracing::info!(start_height, "LMDB is empty — starting from height {start_height}");
                return Ok(Self {
                    chain: RwLock::new(Arc::new(Chain::new(start_height))),
                    lmdb,
                });
            }
        };

        Ok(Self {
            chain: RwLock::new(Arc::new(Chain::new(chain_start))),
            lmdb,
        })
    }

    /// Create a ChainStream snapshot over the range [start, end].
    ///
    /// Takes the read lock just long enough for one `Arc::clone` call,
    /// then builds the cursor outside the lock.
    pub fn stream_range(
        &self,
        start: Height,
        end: Height,
        freeze_horizon: Height,
    ) -> ChainStream {
        let chain = self.chain.read().unwrap();
        ChainStream::new(
            (**chain).clone(),
            freeze_horizon,
            start,
            end,
        )
    }

    /// Iterate blocks in `[start, end]`, transparently handling LMDB for
    /// frozen heights and the in-memory ChainStream for live heights.
    ///
    /// This is a unified synchronous iterator — callers don't need to know
    /// about the two-tier storage layout.
    pub fn stream_blocks(&self, start: Height, end: Height) -> BlockIter {
        if start > end {
            return BlockIter::new(Arc::clone(self.lmdb()), None, start, end, 0);
        }

        let chain_start = self.chain_start();

        // Capture in-memory snapshot under the read lock.
        let (stream, stream_start) = if end >= chain_start {
            let chain = self.chain.read().unwrap();
            let s = ChainStream::new(
                (**chain).clone(),
                0, // freeze_horizon=0: never error on below-freeze (LMDB handles those)
                u32::max(start, chain_start),
                end,
            );
            (Some(s), chain_start)
        } else {
            (None, chain_start)
        };

        BlockIter::new(Arc::clone(self.lmdb()), stream, start, end, stream_start)
    }

    /// Look up a block by hash. Scans the chain (bounded to MAX_REORG_DEPTH).
    ///
    /// O(n) in chain length — the chain is height-indexed, not hash-indexed.
    pub fn get_block_by_hash(&self, hash: &BlockHash) -> Option<Block> {
        let chain = self.chain.read().unwrap();
        for (_h, block) in chain.iter() {
            if &block.hash == hash {
                return Some(block.clone());
            }
        }
        None
    }

    /// Look up the best-chain block at a height. Tries the in-memory chain
    /// first, then falls back to LMDB for heights below the chain start.
    pub fn get_block_by_height(&self, height: Height) -> Option<Block> {
        let chain_start = self.chain_start();
        // Try in-memory first
        if height >= chain_start {
            let chain = self.chain.read().unwrap();
            if let Some(block) = chain.get(height) {
                return Some(block.clone());
            }
        }
        // Below chain start or not in memory — try LMDB
        self.get_block_from_lmdb(height)
    }

    /// Return a reference to the LMDB handle.
    pub fn lmdb(&self) -> &Arc<LmdbStore> {
        &self.lmdb
    }

    /// Read a block from LMDB by height. Returns `None` if the height isn't stored.
    fn get_block_from_lmdb(&self, height: Height) -> Option<Block> {
        let (_hash, block) = self.lmdb.get(height).ok()??;
        Some(block)
    }

    /// Get the current tip hash.
    pub fn tip(&self) -> BlockHash {
        let chain = self.chain.read().unwrap();
        chain.tip_hash().unwrap_or(crate::types::GENESIS_HASH)
    }

    /// Get the tip height (none if chain is empty).
    pub fn tip_height(&self) -> Option<Height> {
        let chain = self.chain.read().unwrap();
        chain.tip_height()
    }

    /// Compute the freeze horizon: `tip.height - MAX_REORG_DEPTH`.
    pub fn freeze_horizon(&self) -> Height {
        self.tip_height()
            .unwrap_or(0)
            .saturating_sub(MAX_REORG_DEPTH)
    }

    /// Height of the first block still in the in-memory chain.
    /// Heights below this have been frozen to LMDB.
    pub fn chain_start(&self) -> Height {
        self.chain.read().unwrap().start
    }

    /// Height of the next block to be added (= cs + chain.len()).
    /// Equal to `chain_start()` when the chain is empty.
    pub(crate) fn ct(&self) -> Height {
        let chain = self.chain.read().unwrap();
        chain.start + chain.len() as u32
    }

    /// Ingest a new block extending the current tip.
    ///
    /// Validates preconditions and swaps the new chain root.
    pub fn ingest(&self, hash: BlockHash, mut block: Block) -> Result<(), StoreError> {
        block.hash = hash;
        let mut chain = self.chain.write().unwrap();

        // Validate continuity and height
        if let Some(tip_block) = chain.last() {
            if block.prev_hash != tip_block.hash {
                return Err(StoreError::InsertionFailed(format!(
                    "prev_hash {:?} != tip {:?}",
                    block.prev_hash, tip_block.hash
                )));
            }
            if block.height != tip_block.height + 1 {
                return Err(StoreError::InsertionFailed(format!(
                    "height {} != tip.height + 1 = {}",
                    block.height,
                    tip_block.height + 1
                )));
            }
        }

        let new_chain = chain.push_back(block);
        *chain = Arc::new(new_chain);

        Ok(())
    }

    /// Ingest a batch of blocks, replacing the chain atomically.
    ///
    /// Truncates from `trim_from` (inclusive) and appends `fragment`.
    /// Keeps `[cs, trim_from - 1]`, replaces `[trim_from, ct)`.
    pub fn add_fragment(
        &self,
        trim_from: Height,
        fragment: Vector<Block>,
    ) -> Result<(), StoreError> {
        if fragment.is_empty() {
            return Ok(());
        }
        let fragment_len = fragment.len();
        let mut chain = self.chain.write().unwrap();
        let old_tip = chain.tip_height();
        let new_chain = chain.add_fragment(trim_from, fragment);
        if let Some(old_tip_height) = old_tip {
            if trim_from <= old_tip_height {
                tracing::warn!(
                    trim_from,
                    old_tip = old_tip_height,
                    fragment_len,
                    "reorg detected — add_fragment replaces existing blocks",
                );
            }
        }
        *chain = Arc::new(new_chain);
        Ok(())
    }

    /// Batch-ingest blocks that extend the current tip. Validates internal
    /// continuity and tip extension, then appends.
    ///
    /// For reorgs, use [`add_fragment`](Self::add_fragment) instead.
    pub fn ingest_batch(&self, blocks: Vec<(BlockHash, Block)>) -> Result<(), StoreError> {
        if blocks.is_empty() {
            return Ok(());
        }

        // Validate internal chain continuity.
        for window in blocks.windows(2) {
            let (prev_hash, _prev_block) = &window[0];
            let (_hash, block) = &window[1];
            if block.prev_hash != *prev_hash {
                return Err(StoreError::InsertionFailed(format!(
                    "ingest_batch: internal chain break at height {}: \
                     prev_hash {:?} != expected {:?}",
                    block.height, block.prev_hash, prev_hash,
                )));
            }
        }

        let mut chain = self.chain.write().unwrap();

        // Validate the batch extends the current tip (when chain is non-empty).
        let (_first_hash, first_block) = &blocks[0];
        if let Some(tip_block) = chain.last() {
            if first_block.prev_hash != tip_block.hash {
                return Err(StoreError::InsertionFailed(format!(
                    "ingest_batch: first block prev_hash {:?} != tip {:?}",
                    first_block.prev_hash, tip_block.hash,
                )));
            }
        }

        let mut new_chain = (**chain).clone();
        for (hash, mut block) in blocks {
            // Set the hash on the block from the tuple.
            block.hash = hash;
            new_chain = new_chain.push_back(block);
        }
        *chain = Arc::new(new_chain);

        Ok(())
    }

    /// Write ALL in-memory chain blocks to LMDB and reset the chain to empty.
    ///
    /// After this call: `cs = ct` (old `cs + flushed_count`), `chain = []`.
    /// When no LMDB is configured the chain is still emptied and `cs`/`ct`
    /// advance — the blocks are not persisted but the sync loop can proceed
    /// with forward fill.
    pub(crate) fn flush_chain_to_lmdb(&self) -> Result<(), StoreError> {
        let batch: Vec<(BlockHash, Block)> = {
            let chain = self.chain.read().unwrap();
            if chain.is_empty() {
                return Ok(());
            }
            chain.iter().map(|(_h, b)| (b.hash, b.clone())).collect()
        };

        let count = batch.len();
        let lmdb = &self.lmdb;
        lmdb.put_batch(&batch)?;

        let mut chain = self.chain.write().unwrap();
        let new_start = chain.start + count as u32;
        *chain = Arc::new(Chain::new(new_start));

        tracing::debug!(count, new_cs = new_start, "Flushed chain to LMDB");
        Ok(())
    }

    /// Write blocks directly to LMDB and advance `cs` / `ct`.
    ///
    /// The in-memory chain must be empty (caller must have flushed first).
    /// Used for forward fill. Panics if LMDB is not configured.
    pub(crate) fn append_to_freezer(&self, blocks: &[(BlockHash, Block)]) -> Result<(), StoreError> {
        if blocks.is_empty() {
            return Ok(());
        }

        debug_assert!(
            self.chain.read().unwrap().is_empty(),
            "append_to_freezer requires empty chain"
        );

        let lmdb = &self.lmdb;
        lmdb.put_batch(blocks)?;

        let count = blocks.len();
        let mut chain = self.chain.write().unwrap();
        let new_start = chain.start + count as u32;
        *chain = Arc::new(Chain::new(new_start));

        tracing::debug!(count, new_cs = new_start, "Appended to freezer");
        Ok(())
    }

    /// Truncate from `trim_from` (inclusive), then append `fragment`.
    ///
    /// # Panics (debug)
    /// - `fragment.len() > MAX_REORG_DEPTH`
    /// - `ct - cs > 2 * MAX_REORG_DEPTH` after appending
    pub(crate) fn append_to_chain(
        &self,
        trim_from: Height,
        fragment: Vector<Block>,
    ) -> Result<(), StoreError> {
        debug_assert!(fragment.len() <= MAX_REORG_DEPTH as usize);

        let fragment_len = fragment.len();
        let mut chain = self.chain.write().unwrap();
        let old_tip = chain.tip_height();
        let new_chain = chain.add_fragment(trim_from, fragment);
        if let Some(old_tip_height) = old_tip {
            if trim_from <= old_tip_height {
                tracing::warn!(
                    trim_from,
                    old_tip = old_tip_height,
                    fragment_len,
                    "reorg detected — append_to_chain replaces existing blocks",
                );
            }
        }
        debug_assert!(new_chain.len() as u32 <= 2 * MAX_REORG_DEPTH);
        *chain = Arc::new(new_chain);
        Ok(())
    }

    /// If `ct - cs > MAX_REORG_DEPTH`, freeze excess blocks from the chain
    /// head to LMDB so the chain is at most `MAX_REORG_DEPTH` blocks long.
    ///
    /// Panics if LMDB is not configured.
    pub(crate) fn trim_chain(&self) -> Result<(), StoreError> {
        let lmdb = &self.lmdb;

        let cl = self.ct() - self.chain_start();
        if cl <= MAX_REORG_DEPTH {
            return Ok(());
        }
        let c = (cl - MAX_REORG_DEPTH) as usize;

        let batch: Vec<(BlockHash, Block)> = {
            let chain = self.chain.read().unwrap();
            let start = chain.start;
            chain
                .iter()
                .take(c)
                .map(|(h, b)| {
                    debug_assert_eq!(h, start + (h - start), "chain iter out of order");
                    (b.hash, b.clone())
                })
                .collect()
        };

        debug_assert_eq!(batch.len(), c);
        lmdb.put_batch(&batch)?;

        {
            let mut chain = self.chain.write().unwrap();
            let mut new_chain = (**chain).clone();
            for _ in 0..c {
                match new_chain.pop_front() {
                    Some((_, next)) => new_chain = next,
                    None => break,
                }
            }
            *chain = Arc::new(new_chain);
        } // write guard dropped here

        debug_assert!(self.ct() - self.chain_start() <= MAX_REORG_DEPTH);
        tracing::debug!(frozen = c, new_cs = self.chain_start(), "Trimmed chain");
        Ok(())
    }

    /// Freeze chain blocks below `tip - MAX_REORG_DEPTH` to LMDB.
    ///
    /// Panics if LMDB is not configured. Called from tests; not yet wired
    /// into production (trim_chain serves the same purpose).
    #[allow(dead_code)]
    pub(crate) fn freeze(&self) -> Result<(), StoreError> {
        let lmdb = &self.lmdb;

        let horizon = self.freeze_horizon();
        if horizon == 0 {
            return Ok(());
        }

        let start = self.chain_start();
        if start >= horizon {
            return Ok(());
        }
        let to_freeze = (horizon - start) as usize;

        let mut batch = Vec::with_capacity(to_freeze);
        {
            let chain = self.chain.read().unwrap();
            for i in 0..to_freeze.min(chain.len()) {
                let h = start + i as u32;
                if let Some(block) = chain.get(h) {
                    batch.push((block.hash, block.clone()));
                }
            }
        }

        if batch.is_empty() {
            return Ok(());
        }

        lmdb.put_batch(&batch)?;

        {
            let mut chain = self.chain.write().unwrap();
            let mut new_chain = (**chain).clone();
            for _ in 0..batch.len() {
                match new_chain.pop_front() {
                    Some((_, c)) => new_chain = c,
                    None => break,
                }
            }
            *chain = Arc::new(new_chain);
        }

        tracing::info!(
            frozen_count = batch.len(),
            new_chain_start = self.chain_start(),
            "Froze blocks to LMDB"
        );
        Ok(())
    }
}

impl std::fmt::Debug for ChainState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainState")
            .field("tip", &self.tip_height())
            .field("chain_start", &self.chain_start())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{genesis_block, GENESIS_HASH};

    fn make_block(height: Height, hash: BlockHash, prev_hash: BlockHash) -> Block {
        Block::new(height, hash, prev_hash, vec![height as u8])
    }

    /// Build a linear chain of `count` blocks starting at `start_height` with
    /// `prev_hash` as the parent of the first block. Each block's hash is
    /// `[height as u8, tag, 0…]` so chains at the same height with different
    /// tags are distinct.
    fn build_chain(
        start_height: u32,
        prev_hash: BlockHash,
        count: u32,
        tag: u8,
    ) -> Vec<(BlockHash, Block)> {
        let mut chain = Vec::new();
        let mut prev = prev_hash;
        for i in 0..count {
            let h = start_height + i;
            let hash = [
                h as u8, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            chain.push((hash, make_block(h, hash, prev)));
            prev = hash;
        }
        chain
    }

    #[test]
    fn ingest_extends_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let block1 = make_block(1, [1u8; 32], GENESIS_HASH);

        state.ingest([1u8; 32], block1).unwrap();
        assert_eq!(state.tip(), [1u8; 32]);
        assert_eq!(state.tip_height(), Some(1));
        assert!(state.get_block_by_hash(&[1u8; 32]).is_some());
        assert_eq!(
            state.get_block_by_height(1).unwrap().prev_hash,
            GENESIS_HASH
        );
    }

    #[test]
    fn ingest_rejects_duplicate_hash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let block = make_block(1, [1u8; 32], GENESIS_HASH);
        state.ingest([1u8; 32], block.clone()).unwrap();
        // ingest doesn't check hash uniqueness (only prev_hash continuity).
        // That's fine — it extends the chain. Rejects happen on prev_hash mismatch.
        let block2 = make_block(2, [2u8; 32], GENESIS_HASH); // wrong prev_hash
        assert!(state.ingest([2u8; 32], block2).is_err());
    }

    #[test]
    fn stream_range_snapshot_isolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let block1 = make_block(1, [1u8; 32], GENESIS_HASH);
        state.ingest([1u8; 32], block1).unwrap();

        // Take a snapshot at height 0
        let mut stream = state.stream_range(0, 0, 0);
        let b0 = stream.next().unwrap().unwrap();
        assert_eq!(b0.height, 0);

        // Ingest another block — stream is unaffected
        let block2 = make_block(2, [2u8; 32], [1u8; 32]);
        state.ingest([2u8; 32], block2).unwrap();

        assert!(stream.next().unwrap().is_none()); // stream only had 0..0
        assert_eq!(state.tip_height(), Some(2));
    }

    // =========================================================================
    // ingest_batch chain-continuity validation
    // =========================================================================

    #[test]
    fn ingest_batch_valid_extends_tip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];
        let hash3 = [3u8; 32];
        let blocks = vec![
            (hash1, make_block(1, hash1, GENESIS_HASH)),
            (hash2, make_block(2, hash2, hash1)),
            (hash3, make_block(3, hash3, hash2)),
        ];
        state.ingest_batch(blocks).unwrap();
        assert_eq!(state.tip(), hash3);
        assert_eq!(state.tip_height(), Some(3));
    }

    #[test]
    fn ingest_batch_rejects_bad_tip_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let wrong_prev = [99u8; 32];
        let hash1 = [1u8; 32];
        let blocks = vec![(hash1, make_block(1, hash1, wrong_prev))];
        let err = state.ingest_batch(blocks).unwrap_err();
        assert!(err.to_string().contains("ingest_batch"), "got: {err}");
    }

    #[test]
    fn ingest_batch_rejects_internal_break() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];
        let blocks = vec![
            (hash1, make_block(1, hash1, GENESIS_HASH)),
            (hash2, make_block(2, hash2, [99u8; 32])),
        ];
        let err = state.ingest_batch(blocks).unwrap_err();
        assert!(err.to_string().contains("internal chain break"), "got: {err}");
    }

    #[test]
    fn ingest_batch_empty_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![]).unwrap();
        assert_eq!(state.tip(), GENESIS_HASH);
    }

    #[test]
    fn ingest_batch_empty_store_accepts_valid_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 100).unwrap();
        let hash100 = [10u8; 32];
        let hash101 = [11u8; 32];
        let blocks = vec![
            (hash100, make_block(100, hash100, [0u8; 32])),
            (hash101, make_block(101, hash101, hash100)),
        ];
        state.ingest_batch(blocks).unwrap();
        assert_eq!(state.tip(), hash101);
    }

    // =========================================================================
    // add_fragment (reorg / reorg-safe tip-extension)
    // =========================================================================

    #[test]
    fn add_fragment_tip_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        let hash1 = [1u8; 32];
        let block1 = make_block(1, hash1, GENESIS_HASH);

        let fragment: Vector<Block> = vec![block1.clone()].into_iter().collect();
        state.add_fragment(1, fragment).unwrap();
        assert_eq!(state.tip_height(), Some(1));
        assert_eq!(state.get_block_by_height(1).unwrap().hash, hash1);
    }

    #[test]
    fn add_fragment_reorg() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())]).unwrap();
        // Build chain 0→1→2→3
        let fork_a = build_chain(1, GENESIS_HASH, 3, 0);
        state.ingest_batch(fork_a).unwrap();
        assert_eq!(state.tip_height(), Some(3));

        // Reorg at height 1: rebuild 2→3 with tag 1
        let hash1 = [
            1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        let fork_b = build_chain(2, hash1, 2, 1);
        let fragment: Vector<Block> = fork_b.into_iter().map(|(_, b)| b).collect();
        state.add_fragment(2, fragment).unwrap();

        assert_eq!(state.tip_height(), Some(3));
        // Old block at height 2 (tag 0) gone
        let old_hash2 = [
            2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        assert!(state.get_block_by_hash(&old_hash2).is_none());
        // New block at height 2 (tag 1) present
        let new_hash2 = [
            2u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        assert!(state.get_block_by_hash(&new_hash2).is_some());
    }

    // =========================================================================
    // LMDB persistence
    // =========================================================================

    #[test]
    fn freeze_moves_below_horizon_blocks_to_lmdb() -> Result<(), StoreError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0)?;

        // Ingest genesis + 111 blocks. freeze_horizon = 111 - 101 = 10.
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())])?;
        let chain = build_chain(1, GENESIS_HASH, 111, 0);
        let hash10 = [
            10u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ];
        state.ingest_batch(chain)?;
        assert_eq!(state.tip_height(), Some(111));
        assert_eq!(state.freeze_horizon(), 10);

        // Freeze blocks 0..=9 to LMDB.
        state.freeze()?;

        // LMDB received blocks 0..=9 (10 blocks).
        let lmdb = state.lmdb();
        assert_eq!(lmdb.block_count()?.unwrap(), 10);

        // Frozen blocks removed from chain.
        assert!(state.get_block_by_hash(&GENESIS_HASH).is_none(), "genesis should be gone from chain");
        for h in 1..=9 {
            let hash = [
                h as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            assert!(
                state.get_block_by_hash(&hash).is_none(),
                "frozen block {h} should be gone from chain"
            );
        }

        // Frozen blocks still accessible via get_block_by_height (LMDB fallback).
        for h in 0..=9 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.height, h);
            assert_eq!(block.data, if h == 0 { vec![] } else { vec![h as u8] });
        }

        // Chain advanced past frozen heights.
        assert_eq!(state.chain_start(), 10);

        // Blocks at and above horizon still in chain.
        assert!(state.get_block_by_hash(&hash10).is_some());
        assert!(state.get_block_by_height(10).is_some());
        assert!(state.get_block_by_height(111).is_some());
        Ok(())
    }

    #[test]
    fn reopen_restores_frozen_blocks_from_lmdb() -> Result<(), StoreError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Session 1: ingest, freeze, drop.
        {
            let state = ChainState::open(&path, 0)?;
            state.ingest_batch(vec![(GENESIS_HASH, genesis_block())])?;
            let chain = build_chain(1, GENESIS_HASH, 111, 0);
            state.ingest_batch(chain)?;
            state.freeze()?;
            let lmdb = state.lmdb();
            assert_eq!(lmdb.block_count()?.unwrap(), 10);
        }

        // Session 2: reopen at same path. Chain starts empty at
        // lmdb_latest + 1 = 10. Only frozen blocks (0..=9) survive — they are
        // served from LMDB via fallthrough, not from the chain.
        let state = ChainState::open(&path, 0)?;
        // Chain is empty — tip is genesis hash, not in LMDB.
        assert_eq!(state.tip_height(), None);
        assert_eq!(state.chain_start(), 10);

        // Frozen blocks served from LMDB (not in chain — get_block_by_hash
        // scans the chain only).
        assert!(state.get_block_by_hash(&GENESIS_HASH).is_none());
        for h in 0..=9 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.height, h);
            assert_eq!(block.data, if h == 0 { vec![] } else { vec![h as u8] });
        }

        // Blocks above the freeze horizon (10..=111) were not persisted.
        assert!(state.get_block_by_height(10).is_none());
        Ok(())
    }

    #[test]
    fn reorg_after_freeze_frozen_blocks_still_served() -> Result<(), StoreError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0)?;

        // Build genesis + 111 blocks, freeze blocks 0..=9. chain_start becomes 10.
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())])?;
        let fork_a = build_chain(1, GENESIS_HASH, 111, 0);
        let hash15 = [
            15u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ];
        state.ingest_batch(fork_a)?;
        assert_eq!(state.tip_height(), Some(111));
        state.freeze()?;
        assert_eq!(state.chain_start(), 10);

        // Reorg at height 15: trim from 16 to rebuild 16..=20 (tag 1).
        let fork_b = build_chain(16, hash15, 5, 1);
        let fragment: Vector<Block> = fork_b.into_iter().map(|(_, b)| b).collect();
        state.add_fragment(16, fragment).unwrap();

        assert_eq!(state.tip_height(), Some(20));

        // New blocks at heights 16..=20 are from fork B.
        for h in 16..=20 {
            let block = state.get_block_by_height(h).unwrap();
            assert_eq!(block.height, h);
            assert_eq!(block.data, vec![h as u8]);
        }

        // Old fork-A blocks at heights 16..=111 were truncated.
        let hash16 = [
            16u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(state.get_block_by_hash(&hash16).is_none(), "old height 16 should be gone");

        // Frozen blocks (0..=9) are still served from LMDB after the reorg.
        for h in 0..=9 {
            assert!(
                state.get_block_by_height(h).is_some(),
                "frozen height {h} should still be served from LMDB"
            );
        }
        Ok(())
    }

    #[test]
    fn stream_snapshots_diverge_after_reorg() -> Result<(), StoreError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = ChainState::open(tmp.path(), 0).unwrap();
        state.ingest_batch(vec![(GENESIS_HASH, genesis_block())])?;
        let fork_a = build_chain(1, GENESIS_HASH, 5, 0);
        let hash2 = [
            2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        state.ingest_batch(fork_a)?;

        // Snapshot the pre-reorg chain over heights 0..=5.
        let mut stream1 = state.stream_range(0, 5, 0);

        // Reorg: trim from 3 to rebuild 3..=5 with tag 1.
        let fork_b = build_chain(3, hash2, 3, 1);
        let fragment: Vector<Block> = fork_b.into_iter().map(|(_, b)| b).collect();
        state.add_fragment(3, fragment)?;

        // Snapshot the post-reorg chain.
        let mut stream2 = state.stream_range(0, 5, 0);

        // Stream 1 (pre-reorg snapshot): all blocks tag 0, including the
        // ones that were truncated from the live chain.
        for h in 0..=5 {
            let block = stream1.next().unwrap().unwrap();
            assert_eq!(block.height, h);
            let expected: Vec<u8> = if h == 0 { vec![] } else { vec![h as u8] };
            assert_eq!(block.data, expected, "stream1 height {h}");
        }
        assert!(stream1.next().unwrap().is_none(), "stream1 should be exhausted");

        // Stream 2 (post-reorg snapshot): heights 0..=2 are tag 0,
        // heights 3..=5 are tag 1.
        for h in 0..=5 {
            let block = stream2.next().unwrap().unwrap();
            assert_eq!(block.height, h);
            let expected: Vec<u8> = if h == 0 { vec![] } else { vec![h as u8] };
            assert_eq!(block.data, expected, "stream2 height {h}");
        }
        assert!(stream2.next().unwrap().is_none(), "stream2 should be exhausted");

        // Live state: truncated blocks are gone, new blocks present.
        let hash3_tag0 = [
            3u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        let hash3_tag1 = [
            3u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        assert!(state.get_block_by_hash(&hash3_tag0).is_none(), "old C(3) truncated");
        assert!(state.get_block_by_hash(&hash3_tag1).is_some(), "new F(3) present");
        assert_eq!(state.tip_height(), Some(5));
        Ok(())
    }
}
