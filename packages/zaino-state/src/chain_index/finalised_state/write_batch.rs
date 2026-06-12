//! Byte-budgeted batching policy for finalised-state block writes.
//!
//! During initial sync every block currently pays one durable LMDB commit (two
//! fsyncs). [`WriteBatcher`] groups contiguous blocks so many blocks share one
//! commit, flushing a batch when either:
//!
//! - the accumulated **byte budget** is reached (an estimate of on-disk write
//!   volume — LMDB tracks roughly 512 MiB of dirty pages per write transaction
//!   before spilling, so the default budget stays well inside that), or
//! - the incoming block **depends on uncommitted state**: the batched write
//!   path (`DbV1::write_blocks`) resolves transparent prevouts and
//!   spent-output bookkeeping against committed state only, so a block that
//!   spends an output created by a pending batch block — or that spends a
//!   sibling output of a transaction another pending block already spends
//!   from — must wait for the batch in front of it to commit first.

use std::collections::HashSet;

use crate::{
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, FixedEncodedLen as _,
    IndexedBlock, TransactionHash, TxInCompact, TxLocation, TxOutCompact,
};

/// Default flush threshold for [`WriteBatcher`].
///
/// Far below LMDB's ~512 MiB per-transaction dirty-page tracking limit even
/// after B-tree overhead, while large enough that per-batch commit cost is
/// negligible against per-block CPU work.
pub(crate) const DEFAULT_WRITE_BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;

/// Accumulates contiguous [`IndexedBlock`]s into batches for
/// `DbV1::write_blocks`, flushing on the byte budget or on intra-batch
/// transparent dependencies (see the module docs for both rules).
pub(crate) struct WriteBatcher {
    byte_budget: usize,
    pending: Vec<IndexedBlock>,
    pending_bytes: usize,
    /// Transaction ids created by blocks in `pending`.
    created_txids: HashSet<TransactionHash>,
    /// Ids of prior transactions whose outputs blocks in `pending` spend.
    spent_from_txids: HashSet<TransactionHash>,
}

impl WriteBatcher {
    pub(crate) fn new(byte_budget: usize) -> Self {
        Self {
            byte_budget,
            pending: Vec::new(),
            pending_bytes: 0,
            created_txids: HashSet::new(),
            spent_from_txids: HashSet::new(),
        }
    }

    /// Adds `block` to the batch and returns a chunk ready to write, if any.
    ///
    /// When `block` completes the byte budget, the returned chunk includes it.
    /// When `block` depends on a pending block (it spends an output created
    /// by — or a sibling output of a transaction spent from by — the current
    /// batch), the pending batch is returned *without* `block`, which seeds
    /// the next batch; committing the returned chunk first preserves the
    /// committed-state-only read contract of the batched write path.
    pub(crate) fn push(&mut self, block: IndexedBlock) -> Option<Vec<IndexedBlock>> {
        let flushed = if self.depends_on_pending(&block) {
            self.take_pending()
        } else {
            None
        };

        self.add(block);

        if flushed.is_some() {
            return flushed;
        }
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

    fn add(&mut self, block: IndexedBlock) {
        self.pending_bytes += estimated_block_write_bytes(&block);
        for tx in block.transactions() {
            self.created_txids.insert(*tx.txid());
            for input in tx.transparent().inputs() {
                if input.is_null_prevout() {
                    continue;
                }
                self.spent_from_txids
                    .insert(TransactionHash::from(*input.prevout_txid()));
            }
        }
        self.pending.push(block);
    }

    fn take_pending(&mut self) -> Option<Vec<IndexedBlock>> {
        if self.pending.is_empty() {
            return None;
        }
        self.pending_bytes = 0;
        self.created_txids.clear();
        self.spent_from_txids.clear();
        Some(std::mem::take(&mut self.pending))
    }

    /// Whether `block` reads state a pending block writes. Same-block spends
    /// are exempt: the write path resolves those from the block itself.
    fn depends_on_pending(&self, block: &IndexedBlock) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let mut own_txids: HashSet<TransactionHash> = HashSet::new();
        for tx in block.transactions() {
            own_txids.insert(*tx.txid());
        }
        block.transactions().iter().any(|tx| {
            tx.transparent().inputs().iter().any(|input| {
                if input.is_null_prevout() {
                    return false;
                }
                let prev_txid = TransactionHash::from(*input.prevout_txid());
                if own_txids.contains(&prev_txid) {
                    return false;
                }
                self.created_txids.contains(&prev_txid)
                    || self.spent_from_txids.contains(&prev_txid)
            })
        })
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
