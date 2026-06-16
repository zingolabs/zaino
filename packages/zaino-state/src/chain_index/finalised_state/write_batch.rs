//! Memory-budgeted batching policy for finalised-state block writes.
//!
//! During initial sync every block previously paid one durable LMDB commit
//! (two fsyncs). [`WriteBatcher`] groups contiguous blocks so many blocks
//! share one commit, flushing a batch when the accumulated **heap budget** is
//! reached — an estimate of the buffered blocks' in-memory size. It is the only
//! flush trigger, so chains of tiny blocks pack as many blocks as fit into one
//! commit, maximising amortisation.
//!
//! The bound is RAM, not LMDB's ~512 MiB dirty-page spill ceiling. The write
//! path inserts the random-keyed `spent` / `txid_location` indexes in sorted key
//! order (see `DbV1::put_block_batch_in_txn`), so a batch that outgrows the dirty
//! list spills *sequentially* — pages written in key order and never re-dirtied,
//! which is cheap. A larger batch therefore sorts more keys together (better
//! B-tree locality, fewer commits) and is bounded only by buffered-block RAM.
//!
//! Transparent dependencies are no flush trigger: the batched write path
//! (`DbV1::write_blocks`) threads a `PendingBatchState` overlay through the
//! batch, so blocks may freely spend outputs created — or sibling outputs of
//! transactions spent from — earlier in the same uncommitted batch.

use crate::IndexedBlock;

/// Default flush threshold for [`WriteBatcher`]: 6 GiB of buffered blocks
/// (marginally safe on a ~64 GiB host — see `DatabaseConfig::sync_write_batch_bytes`).
///
/// A heap budget bounding the buffered `Vec<IndexedBlock>`, not on-disk write
/// volume — under WRITE_MAP the dirty write-set is reclaimable file cache, so the
/// buffer is the binding hard-RAM constraint. Peak batch RAM is ~2–3× this (the
/// buffer plus its encoded `BlockWriteData` and the pending overlay coexist at
/// flush). Tune via `storage.database.sync_write_batch_bytes`.
pub(crate) const DEFAULT_WRITE_BATCH_BYTE_BUDGET: usize = 6 * 1024 * 1024 * 1024;

/// Accumulates contiguous [`IndexedBlock`]s into batches for `DbV1::write_blocks`,
/// flushing when the buffered blocks' estimated heap size reaches the budget. The
/// heap budget is the sole bound: on chains of tiny blocks a batch holds as many
/// as fit, since the per-commit cost amortises best when the most blocks share one
/// commit, and the budget caps peak RAM regardless of block count.
pub(crate) struct WriteBatcher {
    heap_budget: usize,
    pending: Vec<IndexedBlock>,
    pending_heap: usize,
}

impl WriteBatcher {
    pub(crate) fn new(heap_budget: usize) -> Self {
        Self {
            heap_budget,
            pending: Vec::new(),
            pending_heap: 0,
        }
    }

    /// Adds `block` to the batch; returns the batch (including `block`) once it
    /// reaches the heap budget.
    pub(crate) fn push(&mut self, block: IndexedBlock) -> Option<Vec<IndexedBlock>> {
        self.pending_heap += estimated_block_heap_bytes(&block);
        self.pending.push(block);

        if self.pending_heap >= self.heap_budget {
            return self.take_pending();
        }
        None
    }

    /// Removes and returns the pending batch; call once after the final
    /// `push` so no blocks are left behind.
    pub(crate) fn flush(&mut self) -> Option<Vec<IndexedBlock>> {
        self.take_pending()
    }

    fn take_pending(&mut self) -> Option<Vec<IndexedBlock>> {
        if self.pending.is_empty() {
            return None;
        }
        self.pending_heap = 0;
        Some(std::mem::take(&mut self.pending))
    }
}

/// Rough heap-size estimate for one buffered [`IndexedBlock`]: a per-block floor
/// for the header / commitment-tree data / `Vec` overheads, plus a per-transaction
/// term scaled by its input/output/spend/action count. Only used to bound a
/// batch's peak memory, so rough monotonicity with the real footprint matters more
/// than precision (a deliberate over-estimate keeps peak RAM near the budget).
fn estimated_block_heap_bytes(block: &IndexedBlock) -> usize {
    // Block-level heap not attributable to any single transaction (header context,
    // commitment-tree data, the transactions `Vec`'s own allocation, etc.).
    const PER_BLOCK_HEAP: usize = 1024;
    // Per-transaction base plus per-item (input / output / spend / action) heap.
    const PER_TX_HEAP: usize = 256;
    const PER_ITEM_HEAP: usize = 128;

    let tx_heap: usize = block
        .transactions()
        .iter()
        .map(|tx| {
            let transparent = tx.transparent();
            let items = transparent.inputs().len()
                + transparent.outputs().len()
                + tx.sapling().spends().len()
                + tx.sapling().outputs().len()
                + tx.orchard().actions().len();
            PER_TX_HEAP + items * PER_ITEM_HEAP
        })
        .sum();

    PER_BLOCK_HEAP + tx_heap
}
