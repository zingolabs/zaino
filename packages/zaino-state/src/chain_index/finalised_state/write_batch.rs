//! Memory-bounded batching policy for finalised-state block writes.
//!
//! During initial sync every block previously paid one durable LMDB commit
//! (two fsyncs). [`WriteBatcher`] groups contiguous blocks so many blocks
//! share one commit, flushing a batch when the process's **measured resident
//! anonymous memory** (RssAnon, from `/proc/self/status`) reaches the budget.
//! Measuring real RssAnon is the primary flush trigger; the cheap size
//! estimate is kept only as a poll-gate (so `/proc` is read rarely) and as a
//! `/proc`-unavailable fallback bound.
//!
//! ## Why measured, not estimated
//!
//! The earlier policy flushed on an *estimate* of the buffered blocks' heap
//! size. A real mainnet run measured a 16 GiB estimate budget producing 79 GiB
//! of RssAnon — the estimate undercounts real anon by roughly 5×, so the knob
//! was effectively meaningless and risked OOM. The budget is now a bound on
//! measured RssAnon during accumulation, so "6 GiB" means ~6 GiB of
//! accumulation-phase anon, not 5× that.
//!
//! ## Not a hard peak ceiling
//!
//! The budget bounds the *accumulation* phase. It is NOT a hard peak ceiling:
//! during the subsequent `write_blocks` flush the encoded `BlockWriteData` and
//! the `PendingBatchState` overlay coexist with the buffer, so the *peak*
//! RssAnon runs moderately above the budget (observed ~2×). The per-batch
//! commit log prints the real RssAnon so this overshoot is visible and
//! calibratable. Size the budget to leave headroom for that overshoot AND for
//! the page cache: a batch large enough to consume RAM starves the DB
//! working-set cache, so smaller is often faster — bigger is not better.
//!
//! On non-Linux hosts, or when `/proc` is unreadable (sandbox), the batcher
//! falls back to the (undercounting) size estimate.
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

/// Default flush threshold for [`WriteBatcher`]: 6 GiB of measured resident
/// anonymous memory (RssAnon) during batch accumulation — see
/// `DatabaseConfig::sync_write_batch_bytes`.
///
/// This bounds the process's measured RssAnon while a batch accumulates, not an
/// estimate of the buffered blocks' size and not on-disk write volume. The
/// earlier heap estimate undercounted real anon by ~5×; that estimate now serves
/// only as a poll-gate (keeping `/proc` reads cheap) and as a `/proc`-unavailable
/// fallback bound. The real *peak* during the flush runs moderately above this
/// budget (the buffer, its encoded `BlockWriteData`, and the pending overlay
/// coexist at flush — observed ~2×), so the budget is not a hard peak ceiling.
/// Tune via `storage.database.sync_write_batch_bytes`.
pub(crate) const DEFAULT_WRITE_BATCH_BYTE_BUDGET: usize = 6 * 1024 * 1024 * 1024;

/// The process's resident anonymous memory (RssAnon) in bytes, from
/// /proc/self/status. Returns None on any read/parse failure (non-Linux,
/// sandbox), so callers fall back to the size estimate.
pub(crate) fn current_rss_anon_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            // e.g. "RssAnon:\t   12345 kB"
            let kb: usize = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

/// Cadence at which `push` polls real RssAnon, scaled to the budget: read
/// `/proc` once the cheap running estimate advances by this much. Capped at
/// 128 MiB so a large budget still polls often enough to catch the real bound,
/// floored at 64 KiB so a tiny test budget still polls at all.
fn poll_interval(budget: usize) -> usize {
    (budget / 16).clamp(64 * 1024, 128 * 1024 * 1024)
}

/// Accumulates contiguous [`IndexedBlock`]s into batches for `DbV1::write_blocks`,
/// flushing when the process's measured resident anonymous memory (RssAnon)
/// reaches the budget. RssAnon is the primary bound; the cheap size estimate is a
/// poll-gate (so `/proc` is read rarely) and a `/proc`-unavailable fallback. On
/// chains of tiny blocks a batch holds as many as fit under the budget, since the
/// per-commit cost amortises best when the most blocks share one commit.
pub(crate) struct WriteBatcher {
    anon_budget: usize,
    pending: Vec<IndexedBlock>,
    /// Cheap running estimate of buffered-block heap: a poll-gate (when to read
    /// `/proc`) and the `/proc`-unavailable fallback bound.
    pending_estimate: usize,
    /// Poll real RssAnon once `pending_estimate` reaches this.
    next_poll_estimate: usize,
    rss_source: Box<dyn Fn() -> Option<usize> + Send>,
}

impl WriteBatcher {
    pub(crate) fn new(anon_budget: usize) -> Self {
        Self::with_rss_source(anon_budget, Box::new(current_rss_anon_bytes))
    }

    /// Constructs a batcher with an injectable RssAnon source. Production uses
    /// [`current_rss_anon_bytes`] via [`WriteBatcher::new`]; tests inject a
    /// scripted source to exercise the bound deterministically (a source of
    /// `|| None` forces the estimate-fallback path).
    pub(crate) fn with_rss_source(
        anon_budget: usize,
        rss_source: Box<dyn Fn() -> Option<usize> + Send>,
    ) -> Self {
        let anon_budget = anon_budget.max(1);
        Self {
            anon_budget,
            pending: Vec::new(),
            pending_estimate: 0,
            next_poll_estimate: poll_interval(anon_budget),
            rss_source,
        }
    }

    /// Adds `block` to the batch; returns the batch (including `block`) once the
    /// measured RssAnon reaches the budget (or, when `/proc` is unavailable, once
    /// the size estimate does).
    pub(crate) fn push(&mut self, block: IndexedBlock) -> Option<Vec<IndexedBlock>> {
        self.pending_estimate += estimated_block_heap_bytes(&block);
        self.pending.push(block);

        if self.should_flush_now() {
            return self.take_pending();
        }
        None
    }

    /// Decides whether the just-pushed block should trigger a flush, updating the
    /// poll gate as a side effect. Two bounds:
    ///
    /// 1. Estimate backstop (checked every push, cheap): if even the
    ///    undercounting estimate reaches the budget, flush. This is also the
    ///    `/proc`-unavailable bound.
    /// 2. Primary RssAnon bound: once the estimate clears the poll gate, read
    ///    real RssAnon (gated to keep `/proc` reads cheap) and flush if it
    ///    reaches the budget. On Linux this fires well before the estimate
    ///    backstop, since the estimate undercounts.
    fn should_flush_now(&mut self) -> bool {
        if self.pending_estimate >= self.anon_budget {
            return true;
        }

        if self.pending_estimate >= self.next_poll_estimate {
            self.next_poll_estimate = self.pending_estimate + poll_interval(self.anon_budget);
            if let Some(rss) = (self.rss_source)() {
                if rss >= self.anon_budget {
                    return true;
                }
            }
        }

        false
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
        self.pending_estimate = 0;
        self.next_poll_estimate = poll_interval(self.anon_budget);
        Some(std::mem::take(&mut self.pending))
    }
}

/// Rough heap-size estimate for one buffered [`IndexedBlock`]: a per-block floor
/// for the header / commitment-tree data / `Vec` overheads, plus a per-transaction
/// term scaled by its input/output/spend/action count. Used only as the batcher's
/// poll-gate (when to read real RssAnon) and as the `/proc`-unavailable fallback
/// bound, so rough monotonicity with the real footprint matters more than
/// precision.
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

#[cfg(test)]
impl WriteBatcher {
    /// Test-only: advance the cheap running estimate by `bytes`, mirroring what
    /// `push` does without needing a real `IndexedBlock`. Returns whether the new
    /// estimate trips a flush (estimate backstop or polled RssAnon bound).
    fn advance_estimate_and_check(&mut self, bytes: usize) -> bool {
        self.pending_estimate += bytes;
        self.should_flush_now()
    }
}

#[cfg(test)]
mod rss_anon_bound {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn flushes_when_injected_rss_crosses_budget() {
        // The estimate never reaches the budget on its own (each advance is one
        // poll_interval step, well under the 1 MiB budget), so the only path to a
        // flush is the injected RssAnon source rising across the budget.
        let budget = 1024 * 1024; // 1 MiB
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_source = Arc::clone(&counter);
        // Each poll raises reported RssAnon by 256 KiB; after the 4th poll it
        // reaches the 1 MiB budget.
        const RSS_STEP: usize = 256 * 1024;
        let mut batcher = WriteBatcher::with_rss_source(
            budget,
            Box::new(move || {
                Some(counter_for_source.fetch_add(RSS_STEP, Ordering::SeqCst) + RSS_STEP)
            }),
        );

        let step = poll_interval(budget); // clears the poll gate on every advance
        let mut flushed = false;
        for _ in 0..100 {
            if batcher.advance_estimate_and_check(step) {
                flushed = true;
                break;
            }
        }
        assert!(
            flushed,
            "rising injected RssAnon must cross the budget and flush"
        );
        // The flush fired on the poll that first reported RssAnon >= budget, i.e.
        // the 4th poll (4 * 256 KiB == 1 MiB), not the estimate backstop.
        assert_eq!(counter.load(Ordering::SeqCst), budget);
    }

    #[test]
    fn falls_back_to_estimate_when_rss_unavailable() {
        // With a /proc-unavailable source (|| None), the only bound is the
        // estimate backstop: flush once the cumulative estimate reaches the
        // budget. A single advance to exactly the budget must trip it.
        let budget = 64 * 1024;
        let mut batcher = WriteBatcher::with_rss_source(budget, Box::new(|| None));

        assert!(
            !batcher.advance_estimate_and_check(budget - 1),
            "below the budget the estimate backstop must not flush"
        );
        assert!(
            batcher.advance_estimate_and_check(1),
            "reaching the budget with no RssAnon source must flush via the estimate backstop"
        );
    }

    #[test]
    fn poll_interval_clamps_to_floor_and_ceiling() {
        // Floor: a tiny budget still polls at the 64 KiB floor.
        assert_eq!(poll_interval(0), 64 * 1024);
        assert_eq!(poll_interval(1), 64 * 1024);
        assert_eq!(poll_interval(16 * 1024), 64 * 1024);

        // Mid-range: budget / 16 when between floor and ceiling.
        let mid = 64 * 1024 * 1024; // /16 = 4 MiB, within [64 KiB, 128 MiB]
        assert_eq!(poll_interval(mid), mid / 16);

        // Ceiling: a huge budget caps the cadence at 128 MiB.
        assert_eq!(poll_interval(usize::MAX), 128 * 1024 * 1024);
        assert_eq!(poll_interval(64 * 1024 * 1024 * 1024), 128 * 1024 * 1024);
    }
}
