//! Store metrics, labelled `index` = the store's index kind (LSM sets: `lsm/emit.rs`)

use std::time::Duration;

use metrics::Unit;
use zaino_primitives::metric::Metric;

use crate::lsm::metric_names as lsm;

mod metric_names {
    pub(super) const COMMIT_SECONDS: &str = "zaino.store.commit_seconds";
    pub(super) const COMMIT_BYTES_TOTAL: &str = "zaino.store.commit_bytes_total";
}

/// Every metric this crate emits (store + LSM)
///
/// - commits: ms at the tip → minutes on a stalled disk
/// - merges: ms at tier 0 → an hour at the top
pub const METRICS: &[Metric] = &[
    Metric::histogram(
        metric_names::COMMIT_SECONDS,
        &[1e-3, 5e-3, 1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0],
        "One durable commit: every table written + synced, then the manifest, by index",
    )
    .unit(Unit::Seconds),
    Metric::counter(
        metric_names::COMMIT_BYTES_TOTAL,
        "Record + row bytes made durable by commits (before encoding), by index",
    )
    .unit(Unit::Bytes),
    Metric::gauge(
        lsm::SEGMENTS,
        "Committed segments a read searches, by index, table and size tier (⌊log_fanout rows⌋)",
    ),
    Metric::gauge(
        lsm::MERGING,
        "1 = a background merge of this size tier is running, by index, table and tier",
    ),
    Metric::gauge(
        lsm::STALL_AT,
        "Segments a merging tier may list before a commit waits on its merge, by index and table",
    ),
    Metric::counter(
        lsm::BATCH_ROWS_TOTAL,
        "Rows written as new segments by commits, by index and table",
    ),
    Metric::counter(
        lsm::MERGE_ROWS_TOTAL,
        "Rows rewritten by finished background merges, by index and table",
    ),
    Metric::counter(
        lsm::MERGE_BYTES_TOTAL,
        "Segment bytes written by finished background merges, by index and table",
    )
    .unit(Unit::Bytes),
    Metric::counter(
        lsm::MERGE_CANCELLED_TOTAL,
        "Key value + tombstone pairs dropped by finished background merges, by index and table",
    ),
    Metric::histogram(
        lsm::MERGE_DURATION_SECONDS,
        &[1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1200.0, 3600.0],
        "One background merge, read through sealed and linked, by index, table and input tier",
    )
    .unit(Unit::Seconds),
    Metric::histogram(
        lsm::STALL_DURATION_SECONDS,
        &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0],
        "Commit time spent waiting on a merge that fell two windows behind, by index and table",
    )
    .unit(Unit::Seconds),
];

pub(crate) fn committed(index: &'static str, bytes: usize, took: Duration) {
    metrics::histogram!(metric_names::COMMIT_SECONDS, "index" => index).record(took.as_secs_f64());
    metrics::counter!(metric_names::COMMIT_BYTES_TOTAL, "index" => index).increment(bytes as u64);
}
