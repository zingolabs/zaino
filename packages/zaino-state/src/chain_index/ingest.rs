//! Per-block ingest accounting, shared by both [`IndexedBlock`] build paths.
//!
//! - Two loops: finalised writer (below `tip - OPERATIONAL_NFS_DEPTH`), NFS window
//!   at the tip. Catch-up = former, steady state = latter
//!
//! # Cost model: three disjoint spans, no nested total
//!
//! ```text
//!   block_fetch_seconds       source read returning a block
//!   treestate_fetch_seconds   source read of the commitment-tree roots
//!   block_assemble_seconds    zaino's conversion into an IndexedBlock
//! ```
//!
//! - Per-block cost = the sum; nothing derived by subtraction
//! - Prior `block_build_seconds` claimed to enclose the other two — true on the
//!   finalised path, false on NFS (fetch = the `while let` scrutinee, timer starts
//!   after) → `build - fetch - treestate` went negative on the dominant stage,
//!   silently, both stages sharing one name behind a label
//! - Counts unequal by design: a reorg rebuild re-assembles without re-fetching

use crate::chain_index::types::IndexedBlock;

/// Which ingest loop did the work; labels every per-block metric.
///
/// - Migration split out: same read cost, but advances no frontier, so folding it
///   into `finalised` would inflate that stage's block rate
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngestStage {
    Finalised,
    Migration,
}

impl IngestStage {
    /// Label value; on the wire, so stable.
    ///
    /// - Read from `INGEST_STAGES`, not respelled (zainod seeds counters off it)
    #[cfg(feature = "prometheus")]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            IngestStage::Finalised => crate::metric_names::INGEST_STAGES[0],
            IngestStage::Migration => crate::metric_names::INGEST_STAGES[1],
        }
    }
}

/// Protocol work in one block, per op class, from a single transaction walk.
///
/// - One walk feeds [`approx_bytes`](Self::approx_bytes) and [`record`](Self::record);
///   ungated, since the byte estimate is always needed
/// - Directions apart despite equal deserialize cost: outputs are checkable
///   against note-commitment tree growth, and totals re-add where counts cannot
#[derive(Clone, Copy, Default)]
pub(crate) struct BlockWork {
    transactions: u64,
    transparent_inputs: u64,
    transparent_outputs: u64,
    sapling_spends: u64,
    sapling_outputs: u64,
    orchard_actions: u64,
    ironwood_actions: u64,
}

impl BlockWork {
    /// Tally one block in a single pass.
    ///
    /// - No Sprout JoinSplits: absent from the stored compact tx model, so a
    ///   pre-Sapling range reports transparent work only
    pub(crate) fn tally(block: &IndexedBlock) -> Self {
        let mut work = Self {
            transactions: block.transactions().len() as u64,
            ..Self::default()
        };
        // Saturating, as the estimator this replaced was: a corrupt source read
        // must degrade a metric, not panic the ingest loop in a debug build
        for tx in block.transactions() {
            let transparent = tx.transparent();
            work.transparent_inputs = work
                .transparent_inputs
                .saturating_add(transparent.inputs().len() as u64);
            work.transparent_outputs = work
                .transparent_outputs
                .saturating_add(transparent.outputs().len() as u64);
            work.sapling_spends = work
                .sapling_spends
                .saturating_add(tx.sapling().spends().len() as u64);
            work.sapling_outputs = work
                .sapling_outputs
                .saturating_add(tx.sapling().outputs().len() as u64);
            work.orchard_actions = work
                .orchard_actions
                .saturating_add(tx.orchard().actions().len() as u64);
            work.ironwood_actions = work
                .ironwood_actions
                .saturating_add(tx.ironwood().actions().len() as u64);
        }
        work
    }

    /// Protocol ops across every pool & direction.
    ///
    /// - Saturating like [`tally`](Self::tally) (a read absorbed there must not panic here)
    fn operations(self) -> u64 {
        self.transparent_inputs
            .saturating_add(self.transparent_outputs)
            .saturating_add(self.sapling_spends)
            .saturating_add(self.sapling_outputs)
            .saturating_add(self.orchard_actions)
            .saturating_add(self.ironwood_actions)
    }

    /// Rough heap size of a buffered [`IndexedBlock`].
    ///
    /// - Bounds the bulk-sync batch in `DbV1::write_blocks_to_height`; approximate
    pub(crate) fn approx_bytes(self) -> u64 {
        self.transactions
            .saturating_mul(256)
            .saturating_add(self.operations().saturating_mul(128))
    }

    /// Publish this block's work against `stage`.
    ///
    /// - Counters resolved per call: labelled, so a handle cache would thread
    ///   through two unrelated loops to save a lookup dwarfed by the deserialize
    #[cfg(feature = "prometheus")]
    pub(crate) fn record(self, stage: IngestStage) {
        use crate::metric_names::*;
        let stage = stage.label();
        for (name, count) in [
            (SYNC_TRANSACTIONS_TOTAL, self.transactions),
            (SYNC_TRANSPARENT_INPUTS_TOTAL, self.transparent_inputs),
            (SYNC_TRANSPARENT_OUTPUTS_TOTAL, self.transparent_outputs),
            (SYNC_SAPLING_SPENDS_TOTAL, self.sapling_spends),
            (SYNC_SAPLING_OUTPUTS_TOTAL, self.sapling_outputs),
            (SYNC_ORCHARD_ACTIONS_TOTAL, self.orchard_actions),
            (SYNC_IRONWOOD_ACTIONS_TOTAL, self.ironwood_actions),
        ] {
            metrics::counter!(name, INGEST_STAGE => stage).increment(count);
        }
    }

    /// No-op without `prometheus`, so call sites need no `cfg`.
    #[cfg(not(feature = "prometheus"))]
    pub(crate) fn record(self, _stage: IngestStage) {}
}

/// One kind of source read: success histogram + miss-counter label, paired.
///
/// - A value, not two call-site args: they must agree, and a block timed with
///   treestate misses is a misattribution nothing at the call site would catch
// Fields read only by the `prometheus` build of `observe`; the constants exist
// either way (named outside any `cfg`)
#[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
#[derive(Clone, Copy)]
pub(crate) struct SourceRead {
    histogram: &'static str,
    kind: &'static str,
}

/// One block: request issued → deserialized in memory.
pub(crate) const BLOCK_READ: SourceRead = SourceRead {
    histogram: crate::metric_names::SYNC_BLOCK_FETCH_SECONDS,
    kind: "block",
};

/// Commitment-tree-root query each block also costs.
pub(crate) const TREESTATE_READ: SourceRead = SourceRead {
    histogram: crate::metric_names::SYNC_TREESTATE_FETCH_SECONDS,
    kind: "treestate",
};

/// Did a source read produce the work its histogram describes, and if not, why.
///
/// - Per-shape, not blanket over `Result<T, E>`: a blanket impl overlaps the
///   `Option` one, and requiring an impl forces a new read shape to state what
///   "produced work" means instead of inheriting `Ok` = yes
pub(crate) trait ReadOutcome {
    /// `None` = produced work; else the `READ_OUTCOME` label naming why not.
    ///
    /// - Bound stays on `observe` in both builds → an unclassifiable read fails
    ///   to compile without `prometheus` too
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    fn miss_reason(&self) -> Option<&'static str>;
}

/// Block read; `Ok(None)` = no block at that height.
impl<T, E> ReadOutcome for Result<Option<T>, E> {
    fn miss_reason(&self) -> Option<&'static str> {
        match self {
            Ok(Some(_)) => None,
            // Normal at the tip (how the NFS loop learns it caught up), fatal
            // below it — a caller's distinction, not the cost histogram's
            Ok(None) => Some("miss"),
            Err(_) => Some("error"),
        }
    }
}

/// Treestate read; a `None` root = unactivated pool, not a miss, so any `Ok` works.
impl<A, B, C, E> ReadOutcome for Result<(A, B, C), E> {
    fn miss_reason(&self) -> Option<&'static str> {
        match self {
            Ok(_) => None,
            Err(_) => Some("error"),
        }
    }
}

/// Await `fut`; time into `read`'s histogram if it produced work, else count a miss.
///
/// - Wraps the await, not each call site: a hoisted `let start` is one early `?`
///   from never recording
/// - Misses excluded, not labelled: NFS ends every pass on a `None`, and at the
///   tip those outnumber real fetches, so the median would time the terminator
pub(crate) async fn observe<F>(_read: SourceRead, _stage: IngestStage, fut: F) -> F::Output
where
    F: std::future::Future,
    F::Output: ReadOutcome,
{
    #[cfg(feature = "prometheus")]
    {
        use crate::metric_names::*;
        let start = std::time::Instant::now();
        let output = fut.await;
        let elapsed = start.elapsed().as_secs_f64();
        match output.miss_reason() {
            None => {
                metrics::histogram!(_read.histogram, INGEST_STAGE => _stage.label()).record(elapsed)
            }
            Some(reason) => metrics::counter!(
                SYNC_FETCH_MISSES_TOTAL,
                INGEST_STAGE => _stage.label(),
                SOURCE_READ => _read.kind,
                READ_OUTCOME => reason,
            )
            .increment(1),
        }
        output
    }
    #[cfg(not(feature = "prometheus"))]
    {
        fut.await
    }
}

/// Times construction → drop into `histogram`. Sync counterpart to [`observe`].
///
/// - Covers every exit path, including the `?` returns a trailing `record` misses
/// - Scope-bound, so disjointness is mechanical: work cannot move in or out
///   without moving the declaration (how the nested total went wrong)
/// - No outcome split: failed work still read what it read. `stage` is `None` for
///   histograms that are not per-block
pub(crate) struct ScopedTimer {
    #[cfg(feature = "prometheus")]
    histogram: &'static str,
    #[cfg(feature = "prometheus")]
    stage: Option<IngestStage>,
    #[cfg(feature = "prometheus")]
    started: std::time::Instant,
}

impl ScopedTimer {
    /// Time an unlabelled histogram. Holds nothing without `prometheus` — no clock
    /// read, drop compiles away.
    pub(crate) fn start(_histogram: &'static str) -> Self {
        Self {
            #[cfg(feature = "prometheus")]
            histogram: _histogram,
            #[cfg(feature = "prometheus")]
            stage: None,
            #[cfg(feature = "prometheus")]
            started: std::time::Instant::now(),
        }
    }

    /// Time a per-block histogram, labelled with the loop doing the work.
    pub(crate) fn staged(_histogram: &'static str, _stage: IngestStage) -> Self {
        Self {
            #[cfg(feature = "prometheus")]
            histogram: _histogram,
            #[cfg(feature = "prometheus")]
            stage: Some(_stage),
            #[cfg(feature = "prometheus")]
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for ScopedTimer {
    fn drop(&mut self) {
        #[cfg(feature = "prometheus")]
        {
            let elapsed = self.started.elapsed().as_secs_f64();
            match self.stage {
                Some(stage) => metrics::histogram!(
                    self.histogram,
                    crate::metric_names::INGEST_STAGE => stage.label(),
                )
                .record(elapsed),
                None => metrics::histogram!(self.histogram).record(elapsed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - Budget bounds bulk-sync peak memory; per-tx → per-block changed the
    ///   expression's shape, must not change its value
    /// - `sum(256 + items*128)` == `256*txs + 128*items`
    #[test]
    fn approx_bytes_matches_the_per_transaction_formula_it_replaced() {
        // (txs, items/tx): empty, coinbase-only, single-pool, mixed
        for (transactions, items_per_tx) in
            [(0u64, 0u64), (1, 0), (1, 7), (12, 3), (4_000, 25), (1, 900)]
        {
            let work = BlockWork {
                transactions,
                // One class: only the op total may affect the estimate
                transparent_inputs: items_per_tx * transactions,
                ..BlockWork::default()
            };
            let legacy: u64 = (0..transactions).map(|_| 256 + items_per_tx * 128).sum();
            assert_eq!(
                work.approx_bytes(),
                legacy,
                "block of {transactions} txs × {items_per_tx} items"
            );
        }
    }

    /// - A missing class understates every rate derived from the sum
    /// - Distinct powers of two catch a field doubled or omitted
    #[test]
    fn operations_sums_every_class_and_excludes_the_transaction_count() {
        let work = BlockWork {
            transactions: 1_000_000,
            transparent_inputs: 1,
            transparent_outputs: 2,
            sapling_spends: 4,
            sapling_outputs: 8,
            orchard_actions: 16,
            ironwood_actions: 32,
        };
        assert_eq!(work.operations(), 63);
    }

    /// - Oracle = the zebra blocks, not the compact model (re-walking
    ///   `IndexedBlock` restates `tally`; the source also catches a lossy convert)
    /// - Per class, not on a total: two classes swapped leaves any total intact
    #[test]
    fn tally_matches_the_zebra_blocks_the_index_was_built_from() {
        let vectors = crate::chain_index::tests::vectors::load_test_vectors()
            .expect("regtest test vectors are checked in beside this crate");
        let mut asserted_shielded = false;

        for (vector, indexed) in
            vectors
                .blocks
                .iter()
                .zip(crate::chain_index::tests::vectors::indexed_block_chain(
                    &vectors.blocks,
                ))
        {
            let work = BlockWork::tally(&indexed);
            let txs = &vector.zebra_block.transactions;

            let expect = |f: fn(&zebra_chain::transaction::Transaction) -> usize| -> u64 {
                txs.iter().map(|tx| f(tx) as u64).sum()
            };

            // One row per tallied field (new pool without a row = visible gap, not a pass)
            for (class, tallied, expected) in [
                ("transaction count", work.transactions, txs.len() as u64),
                (
                    "transparent inputs",
                    work.transparent_inputs,
                    expect(|tx| tx.inputs().len()),
                ),
                (
                    "transparent outputs",
                    work.transparent_outputs,
                    expect(|tx| tx.outputs().len()),
                ),
                (
                    "sapling spends",
                    work.sapling_spends,
                    expect(|tx| tx.sapling_spends_per_anchor().count()),
                ),
                (
                    "sapling outputs",
                    work.sapling_outputs,
                    expect(|tx| tx.sapling_outputs().count()),
                ),
                (
                    "orchard actions",
                    work.orchard_actions,
                    expect(|tx| tx.orchard_actions().count()),
                ),
                (
                    "ironwood actions",
                    work.ironwood_actions,
                    expect(|tx| tx.ironwood_actions().count()),
                ),
            ] {
                assert_eq!(tallied, expected, "{class} at height {}", vector.height);
            }

            asserted_shielded |=
                work.sapling_outputs > 0 || work.orchard_actions > 0 || work.ironwood_actions > 0;
        }

        // Shielded-free vectors pass every assert above while proving nothing
        // about the pools this metric exists for
        assert!(
            asserted_shielded,
            "the test vectors carry no shielded output or action, so the shielded \
             counts were never exercised"
        );
    }

    /// - Shared label = merged series, hiding the double-ingest below the tip and
    ///   a migration's reads against the writer's
    /// - Pins each variant to its `INGEST_STAGES` index (zainod seeds counters off
    ///   it): a new variant emits an unseeded label, a reorder renames two series
    #[cfg(feature = "prometheus")]
    #[test]
    fn every_ingest_stage_has_its_own_label_and_matches_the_published_list() {
        use crate::metric_names::INGEST_STAGES;

        let stages = [IngestStage::Finalised, IngestStage::Migration];
        assert_eq!(
            stages.len(),
            INGEST_STAGES.len(),
            "`IngestStage` and `INGEST_STAGES` disagree on how many stages exist"
        );
        for (index, stage) in stages.iter().enumerate() {
            assert_eq!(
                stage.label(),
                INGEST_STAGES[index],
                "{stage:?} does not sit at index {index} of INGEST_STAGES"
            );
        }

        let mut labels: Vec<&str> = INGEST_STAGES.to_vec();
        labels.sort_unstable();
        let total = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), total, "two ingest stages share a label value");
    }
}
