//! Sliding-window buffer for provisioned block contexts.
//!
//! Sits between the provisioner (supply) and the scheduler/workers
//! (demand). The provisioner pushes block contexts as they arrive;
//! workers read them by global offset; the engine evicts completed
//! batches once all indexes have committed past them.
//!
//! Eviction is per-batch — when the slowest index commits past batch
//! N, all blocks in that batch are dropped together.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::primitives::{BatchIndex, BlockOffset};

/// A buffer of block contexts indexed by global offset.
///
/// Generic over `Ctx` — the provisioner's block context type. Contexts
/// are stored behind `Arc` so workers can hold references without
/// borrowing the buffer.
///
/// The buffer tracks which range of blocks it holds and supports
/// eviction of completed batches.
pub struct BlockBuffer<Ctx> {
    blocks: BTreeMap<u32, Arc<Ctx>>,
    batch_size: u32,
    /// The lowest global offset still in the buffer.
    floor: u32,
}

impl<Ctx> BlockBuffer<Ctx> {
    /// Create a buffer with the given batch size.
    pub fn new(batch_size: u32) -> Self {
        Self {
            blocks: BTreeMap::new(),
            batch_size,
            floor: 0,
        }
    }

    /// Push a block context at the given global offset.
    ///
    /// The offset must be monotonically increasing — blocks arrive in
    /// chain order from the provisioner.
    pub fn push(&mut self, offset: BlockOffset, ctx: Ctx) {
        self.blocks.insert(offset.value(), Arc::new(ctx));
    }

    /// Get a reference-counted handle to the block at `offset`.
    ///
    /// Returns `None` if the block has been evicted or not yet supplied.
    pub fn get(&self, offset: BlockOffset) -> Option<Arc<Ctx>> {
        self.blocks.get(&offset.value()).cloned()
    }

    /// How many blocks are currently in the buffer.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The total number of blocks that have been pushed (including
    /// evicted ones). This is the watermark the engine passes to
    /// `scheduler.set_blocks_available()`.
    pub fn total_pushed(&self) -> u32 {
        self.floor + u32::try_from(self.blocks.len())
            .expect("buffer size fits in u32")
    }

    /// Evict all blocks in batches up to and including `through_batch`.
    ///
    /// Called by the engine when the slowest index has committed past
    /// this batch — the blocks are no longer needed by any index.
    pub fn evict_through_batch(&mut self, through_batch: BatchIndex) {
        let cutoff = (through_batch.value() + 1) * self.batch_size;
        self.blocks = self.blocks.split_off(&cutoff);
        self.floor = cutoff;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(n: u32) -> BlockOffset {
        BlockOffset::new(n)
    }

    fn batch(n: u32) -> BatchIndex {
        BatchIndex::new(n)
    }

    #[test]
    fn push_and_get() {
        let mut buf: BlockBuffer<u32> = BlockBuffer::new(3);
        buf.push(offset(0), 100);
        buf.push(offset(1), 101);
        buf.push(offset(2), 102);

        assert_eq!(*buf.get(offset(0)).expect("exists"), 100);
        assert_eq!(*buf.get(offset(1)).expect("exists"), 101);
        assert_eq!(*buf.get(offset(2)).expect("exists"), 102);
        assert!(buf.get(offset(3)).is_none());
    }

    #[test]
    fn total_pushed_tracks_watermark() {
        let mut buf: BlockBuffer<u32> = BlockBuffer::new(3);
        assert_eq!(buf.total_pushed(), 0);

        buf.push(offset(0), 10);
        buf.push(offset(1), 11);
        assert_eq!(buf.total_pushed(), 2);
    }

    #[test]
    fn evict_drops_completed_batches() {
        let mut buf: BlockBuffer<u32> = BlockBuffer::new(3);
        for i in 0..9 {
            buf.push(offset(i), i * 10);
        }
        assert_eq!(buf.len(), 9);

        // Evict batch 0 (offsets 0, 1, 2).
        buf.evict_through_batch(batch(0));
        assert_eq!(buf.len(), 6);
        assert!(buf.get(offset(0)).is_none());
        assert!(buf.get(offset(2)).is_none());
        assert_eq!(*buf.get(offset(3)).expect("exists"), 30);

        // Evict through batch 1 (offsets 0..6).
        buf.evict_through_batch(batch(1));
        assert_eq!(buf.len(), 3);
        assert!(buf.get(offset(5)).is_none());
        assert_eq!(*buf.get(offset(6)).expect("exists"), 60);
    }

    #[test]
    fn total_pushed_accounts_for_eviction() {
        let mut buf: BlockBuffer<u32> = BlockBuffer::new(3);
        for i in 0..6 {
            buf.push(offset(i), i);
        }
        assert_eq!(buf.total_pushed(), 6);

        buf.evict_through_batch(batch(0));
        // 3 evicted + 3 remaining = 6 total pushed.
        assert_eq!(buf.total_pushed(), 6);
        assert_eq!(buf.len(), 3);
    }
}
