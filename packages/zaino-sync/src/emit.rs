//! Sink + finalized-stream metrics (fetch names = ztest's `zainod` families: a rename breaks its
//! probes)

use std::time::Duration;

use metrics::Unit;
use zaino_primitives::metric::Metric;
use zaino_primitives::types::{Block, Transaction};
use zcash_protocol::consensus::BlockHeight;

mod names {
    pub(super) const FETCH_BLOCKS_TOTAL: &str = "zaino.fetch.blocks_total";
    pub(super) const FETCH_TRANSACTIONS_TOTAL: &str = "zaino.fetch.transactions_total";
    pub(super) const FETCH_TRANSPARENT_INPUTS_TOTAL: &str = "zaino.fetch.transparent_inputs_total";
    pub(super) const FETCH_TRANSPARENT_OUTPUTS_TOTAL: &str =
        "zaino.fetch.transparent_outputs_total";
    pub(super) const FETCH_SAPLING_SPENDS_TOTAL: &str = "zaino.fetch.sapling_spends_total";
    pub(super) const FETCH_SAPLING_OUTPUTS_TOTAL: &str = "zaino.fetch.sapling_outputs_total";
    pub(super) const FETCH_ORCHARD_ACTIONS_TOTAL: &str = "zaino.fetch.orchard_actions_total";
    pub(super) const FETCH_IRONWOOD_ACTIONS_TOTAL: &str = "zaino.fetch.ironwood_actions_total";
    pub(super) const INDEX_APPLIED_HEIGHT: &str = "zaino.index.applied_height";
    pub(super) const INDEX_APPLIED_BLOCKS_TOTAL: &str = "zaino.index.applied_blocks_total";
    pub(super) const INDEX_APPLIED_ROWS_TOTAL: &str = "zaino.index.applied_rows_total";
    pub(super) const INDEX_FINALIZED_HEIGHT: &str = "zaino.index.finalized_height";
    pub(super) const INDEX_RUN_SECONDS: &str = "zaino.index.run_seconds";
    pub(super) const INDEX_WRITE_SECONDS: &str = "zaino.index.write_seconds";
}

/// ms tip runs → minutes for a stalled bulk run
const RUN_BUCKETS: &[f64] = &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0];

/// Every metric this crate emits
pub const METRICS: &[Metric] = {
    use names::*;

    &[
        Metric::counter(
            FETCH_BLOCKS_TOTAL,
            "Blocks sent to the index writers on the finalized stream; a restart counts again",
        ),
        Metric::counter(
            FETCH_TRANSACTIONS_TOTAL,
            "Transactions sent to the index writers on the finalized stream; a restart counts again",
        ),
        Metric::counter(
            FETCH_TRANSPARENT_INPUTS_TOTAL,
            "Transparent inputs sent to the index writers on the finalized stream; a restart counts \
             again",
        ),
        Metric::counter(
            FETCH_TRANSPARENT_OUTPUTS_TOTAL,
            "Transparent outputs sent to the index writers on the finalized stream; a restart \
             counts again",
        ),
        Metric::counter(
            FETCH_SAPLING_SPENDS_TOTAL,
            "Sapling spends sent to the index writers on the finalized stream; a restart counts \
             again",
        ),
        Metric::counter(
            FETCH_SAPLING_OUTPUTS_TOTAL,
            "Sapling outputs sent to the index writers on the finalized stream; a restart counts \
             again",
        ),
        Metric::counter(
            FETCH_ORCHARD_ACTIONS_TOTAL,
            "Orchard actions sent to the index writers on the finalized stream; a restart counts \
             again",
        ),
        Metric::counter(
            FETCH_IRONWOOD_ACTIONS_TOTAL,
            "Ironwood actions sent to the index writers on the finalized stream; a restart counts \
             again",
        ),
        Metric::gauge(
            INDEX_APPLIED_HEIGHT,
            "Highest height the index has folded and applied to its store (committed or not), by \
             index",
        ),
        Metric::counter(
            INDEX_APPLIED_BLOCKS_TOTAL,
            "Blocks the index has folded and applied to its store, by index; a restart counts again",
        ),
        Metric::counter(
            INDEX_APPLIED_ROWS_TOTAL,
            "Records and rows the index has written into its store, by index; a restart counts \
             again",
        ),
        Metric::histogram(
            INDEX_RUN_SECONDS,
            RUN_BUCKETS,
            "One run of finalized blocks through an index writer, taken off its queue → published \
             (fold + write + any wait for input), by index",
        )
        .unit(Unit::Seconds),
        Metric::histogram(
            INDEX_WRITE_SECONDS,
            RUN_BUCKETS,
            "The store side of one run: applies + commits (buffering, segment writes, fsync), by \
             index; run − write = fold + waits",
        )
        .unit(Unit::Seconds),
        Metric::gauge(
            INDEX_FINALIZED_HEIGHT,
            "Highest height the index has durably written (its last commit), by index",
        ),
    ]
};

/// One block's changes applied to `index`'s store
pub(crate) fn applied_block(index: &'static str, rows: usize) {
    metrics::counter!(names::INDEX_APPLIED_BLOCKS_TOTAL, "index" => index).increment(1);
    metrics::counter!(names::INDEX_APPLIED_ROWS_TOTAL, "index" => index).increment(rows as u64);
}

/// `index`'s store: applied through `applied`, durable through `durable` (`None` = nothing yet)
pub(crate) fn index_tips(
    index: &'static str,
    applied: Option<BlockHeight>,
    durable: Option<BlockHeight>,
) {
    if let Some(applied) = applied {
        let height = f64::from(u32::from(applied));
        metrics::gauge!(names::INDEX_APPLIED_HEIGHT, "index" => index).set(height);
    }
    if let Some(durable) = durable {
        let height = f64::from(u32::from(durable));
        metrics::gauge!(names::INDEX_FINALIZED_HEIGHT, "index" => index).set(height);
    }
}

/// `block` sent on the finalized stream
pub(crate) fn handed(block: &Block) {
    let txs = block.transactions();
    let sum = |count: fn(&Transaction) -> usize| txs.iter().map(count).sum::<usize>() as u64;

    metrics::counter!(names::FETCH_BLOCKS_TOTAL).increment(1);
    metrics::counter!(names::FETCH_TRANSACTIONS_TOTAL).increment(txs.len() as u64);
    metrics::counter!(names::FETCH_TRANSPARENT_INPUTS_TOTAL)
        .increment(sum(|tx| tx.transparent.inputs.len()));
    metrics::counter!(names::FETCH_TRANSPARENT_OUTPUTS_TOTAL)
        .increment(sum(|tx| tx.transparent.outputs.len()));
    metrics::counter!(names::FETCH_SAPLING_SPENDS_TOTAL)
        .increment(sum(|tx| tx.sapling.spends.len()));
    metrics::counter!(names::FETCH_SAPLING_OUTPUTS_TOTAL)
        .increment(sum(|tx| tx.sapling.outputs.len()));
    metrics::counter!(names::FETCH_ORCHARD_ACTIONS_TOTAL)
        .increment(sum(|tx| tx.orchard.actions.len()));
    metrics::counter!(names::FETCH_IRONWOOD_ACTIONS_TOTAL)
        .increment(sum(|tx| tx.ironwood.actions.len()));
}

/// One writer run: `run` end to end, `write` of it in the store
pub(crate) fn index_run(index: &'static str, run: Duration, write: Duration) {
    metrics::histogram!(names::INDEX_RUN_SECONDS, "index" => index).record(run.as_secs_f64());
    metrics::histogram!(names::INDEX_WRITE_SECONDS, "index" => index).record(write.as_secs_f64());
}
