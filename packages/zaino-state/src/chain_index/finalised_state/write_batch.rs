//! Byte-budgeted batching policy for finalised-state block writes.
//!
//! During initial sync every block previously paid one durable LMDB commit
//! (two fsyncs). [`WriteBatcher`] groups contiguous blocks so many blocks
//! share one commit, flushing a batch when the accumulated **byte budget** is
//! reached — an estimate of on-disk write volume. The byte budget is the only
//! flush trigger, so chains of tiny blocks pack as many blocks as fit into one
//! commit, maximising amortisation. LMDB tracks roughly 512 MiB of dirty pages
//! per write transaction before spilling, so the default budget stays well
//! inside that even after B-tree overhead.
//!
//! Transparent dependencies are no flush trigger: the batched write path
//! (`DbV1::write_blocks`) threads a `PendingBatchState` overlay through the
//! batch, so blocks may freely spend outputs created — or sibling outputs of
//! transactions spent from — earlier in the same uncommitted batch.

use crate::{
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, FixedEncodedLen as _,
    IndexedBlock, TxInCompact, TxLocation, TxOutCompact,
};

/// Default flush threshold for [`WriteBatcher`].
///
/// Far below LMDB's ~512 MiB per-transaction dirty-page tracking limit even
/// after B-tree overhead, while large enough that per-batch commit cost is
/// negligible against per-block CPU work.
pub(crate) const DEFAULT_WRITE_BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;

/// Accumulates contiguous [`IndexedBlock`]s into batches for
/// `DbV1::write_blocks`, flushing when the estimated write volume reaches the
/// byte budget. The byte budget is the sole bound: on chains of tiny blocks a
/// batch holds as many as fit, since the per-commit cost amortises best when
/// the most blocks share one commit, and the byte budget already caps build
/// memory regardless of block count.
pub(crate) struct WriteBatcher {
    byte_budget: usize,
    pending: Vec<IndexedBlock>,
    pending_bytes: usize,
}

impl WriteBatcher {
    pub(crate) fn new(byte_budget: usize) -> Self {
        Self {
            byte_budget,
            pending: Vec::new(),
            pending_bytes: 0,
        }
    }

    /// Adds `block` to the batch; returns the batch (including `block`) once
    /// it completes the byte budget.
    pub(crate) fn push(&mut self, block: IndexedBlock) -> Option<Vec<IndexedBlock>> {
        self.pending_bytes += estimated_block_write_bytes(&block);
        self.pending.push(block);

        if self.pending_bytes >= self.byte_budget {
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
        self.pending_bytes = 0;
        Some(std::mem::take(&mut self.pending))
    }
}

/// Rough on-disk byte estimate for one block's write: entry payload sizes per
/// table plus per-entry wrapper overhead (version tags, checksums, LMDB
/// keys). Only used to pick batch commit points, so monotonicity with actual
/// write volume matters more than precision.
fn estimated_block_write_bytes(block: &IndexedBlock) -> usize {
    // Per stored entry: 32-byte checksum + wrapper version tag.
    const ENTRY_OVERHEAD: usize = 33;
    // Per-block fixed entries: header (generous upper bound), height,
    // commitment tree data, and the accumulator rewrite.
    const PER_BLOCK_OVERHEAD: usize = 4096;
    // `spent` and `txid_location` entries are keyed by 36-byte outpoints and
    // 32-byte txids respectively; both store a `TxLocation`.
    const OUTPOINT_KEY_LEN: usize = 36;
    const TXID_KEY_LEN: usize = 32;

    let mut bytes = PER_BLOCK_OVERHEAD;
    for tx in block.transactions() {
        // txid-list share plus the reverse `txid_location` entry.
        bytes += TXID_KEY_LEN + (TXID_KEY_LEN + TxLocation::VERSIONED_LEN + ENTRY_OVERHEAD);

        let transparent = tx.transparent();
        // Each non-null input also writes a `spent` entry.
        bytes += transparent.inputs().len()
            * (TxInCompact::VERSIONED_LEN
                + OUTPOINT_KEY_LEN
                + TxLocation::VERSIONED_LEN
                + ENTRY_OVERHEAD);
        bytes += transparent.outputs().len() * TxOutCompact::VERSIONED_LEN;

        let sapling = tx.sapling();
        bytes += sapling.spends().len() * CompactSaplingSpend::VERSIONED_LEN;
        bytes += sapling.outputs().len() * CompactSaplingOutput::VERSIONED_LEN;

        bytes += tx.orchard().actions().len() * CompactOrchardAction::VERSIONED_LEN;
    }
    bytes
}
