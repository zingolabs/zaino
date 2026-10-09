//! Sink + final-stream metrics (fetch names = ztest's `zainod` families: a rename breaks its probes)

use zaino_primitives::types::{Block, Height, Transaction};

const SINK_QUEUE_BYTES: &str = "zaino.sink.queue_bytes";

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
}

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_gauge!(
        SINK_QUEUE_BYTES,
        "Bytes queued for one subscriber, not yet popped, by sink and subscriber; at its budget \
         = that subscriber is holding back the publisher"
    );
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

/// Bytes one subscriber's queue holds: + on push, − on pop (exact permit counts, no sampling)
#[derive(Clone)]
pub(crate) struct QueueBytes {
    gauge: metrics::Gauge,
}

impl QueueBytes {
    pub(crate) fn new(sink: &'static str, subscriber: &'static str) -> Self {
        Self {
            gauge: metrics::gauge!(SINK_QUEUE_BYTES, "sink" => sink, "subscriber" => subscriber),
        }
    }

    pub(crate) fn pushed(&self, bytes: u32) {
        self.gauge.increment(f64::from(bytes));
    }

    pub(crate) fn popped(&self, bytes: usize) {
        self.gauge.decrement(bytes as f64);
    }
}
