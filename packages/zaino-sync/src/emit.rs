//! Sink + final-stream metrics (fetch names = ztest's `zainod` families: a rename breaks its probes)

use std::time::Duration;

use zaino_primitives::types::{Block, Height, Transaction};

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

/// `(metric, bucket edges)` for the exporter (ms tip runs → minutes for a stalled bulk run)
pub const METRIC_BUCKETS: &[(&str, &[f64])] =
    &[(names::INDEX_RUN_SECONDS, RUN_BUCKETS), (names::INDEX_WRITE_SECONDS, RUN_BUCKETS)];

const RUN_BUCKETS: &[f64] = &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0];

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    for (name, what) in [
        (names::FETCH_BLOCKS_TOTAL, "Blocks"),
        (names::FETCH_TRANSACTIONS_TOTAL, "Transactions"),
        (names::FETCH_TRANSPARENT_INPUTS_TOTAL, "Transparent inputs"),
        (names::FETCH_TRANSPARENT_OUTPUTS_TOTAL, "Transparent outputs"),
        (names::FETCH_SAPLING_SPENDS_TOTAL, "Sapling spends"),
        (names::FETCH_SAPLING_OUTPUTS_TOTAL, "Sapling outputs"),
        (names::FETCH_ORCHARD_ACTIONS_TOTAL, "Orchard actions"),
        (names::FETCH_IRONWOOD_ACTIONS_TOTAL, "Ironwood actions"),
    ] {
        metrics::describe_counter!(
            name,
            format!("{what} sent to the index writers on the final stream; a restart counts again")
        );
    }
    metrics::describe_gauge!(
        names::INDEX_APPLIED_HEIGHT,
        "Highest height the index has folded and applied to its store (committed or not), by index"
    );
    metrics::describe_counter!(
        names::INDEX_APPLIED_BLOCKS_TOTAL,
        "Blocks the index has folded and applied to its store, by index; a restart counts again"
    );
    metrics::describe_counter!(
        names::INDEX_APPLIED_ROWS_TOTAL,
        "Records and rows the index has written into its store, by index; a restart counts again"
    );
    metrics::describe_histogram!(
        names::INDEX_RUN_SECONDS,
        metrics::Unit::Seconds,
        "One run of final blocks through an index writer, taken off its queue → published \
         (fold + write + any wait for input), by index"
    );
    metrics::describe_histogram!(
        names::INDEX_WRITE_SECONDS,
        metrics::Unit::Seconds,
        "The store side of one run: applies + commits (buffering, segment writes, fsync), by index; \
         run − write = fold + waits"
    );
    metrics::describe_gauge!(
        names::INDEX_FINALIZED_HEIGHT,
        "Highest height the index has durably written (its last commit), by index"
    );
}

/// One block's changes applied to `index`'s store
pub(crate) fn applied_block(index: &'static str, rows: usize) {
    metrics::counter!(names::INDEX_APPLIED_BLOCKS_TOTAL, "index" => index).increment(1);
    metrics::counter!(names::INDEX_APPLIED_ROWS_TOTAL, "index" => index).increment(rows as u64);
}

/// `index`'s store: applied through `applied`, durable through `durable` (`None` = nothing yet)
pub(crate) fn index_tips(index: &'static str, applied: Option<Height>, durable: Option<Height>) {
    if let Some(applied) = applied {
        let height = f64::from(u32::from(applied));
        metrics::gauge!(names::INDEX_APPLIED_HEIGHT, "index" => index).set(height);
    }
    if let Some(durable) = durable {
        let height = f64::from(u32::from(durable));
        metrics::gauge!(names::INDEX_FINALIZED_HEIGHT, "index" => index).set(height);
    }
}

/// `block` sent on the final stream
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
