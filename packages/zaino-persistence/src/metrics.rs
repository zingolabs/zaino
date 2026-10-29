//! Store metrics, labelled `index` = the store's [`Schema`](crate::Schema) name (LSM sets:
//! `lsm/telemetry.rs`)

use std::time::Duration;

use metrics::Unit;
use zaino_primitives::metric::Metric;

use crate::lsm::metric_names as lsm;

mod metric_names {
    pub(super) const COMMIT_SECONDS: &str = "zaino.store.commit_seconds";
    pub(super) const COMMIT_BYTES_TOTAL: &str = "zaino.store.commit_bytes_total";
}

/// Every metric this crate emits (store + LSM)
pub const METRICS: &[Metric] = &[
    Metric::histogram(
        metric_names::COMMIT_SECONDS,
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
        "Committed segments, summed over the index's maps (a read searches its map's)",
    ),
    Metric::gauge(
        lsm::LIVE_ROWS,
        "Keys the committed segments hold, summed over the index's maps (a `deletes()` map: \
         values minus tombstones)",
    ),
    Metric::gauge(lsm::MERGING, "Background merges running, by index"),
    Metric::counter(
        lsm::MERGE_BYTES_TOTAL,
        "Segment bytes written by finished background merges, by index",
    )
    .unit(Unit::Bytes),
    Metric::histogram(
        lsm::MERGE_DURATION_SECONDS,
        "One background merge, read through sealed and linked, by index",
    )
    .unit(Unit::Seconds),
    Metric::histogram(
        lsm::STALL_DURATION_SECONDS,
        "Commit time spent waiting on a merge that fell two windows behind, by index",
    )
    .unit(Unit::Seconds),
];

pub(crate) fn committed(index: &'static str, bytes: usize, took: Duration) {
    metrics::histogram!(metric_names::COMMIT_SECONDS, "index" => index).record(took.as_secs_f64());
    metrics::counter!(metric_names::COMMIT_BYTES_TOTAL, "index" => index)
        .increment(u64::try_from(bytes).expect("usize fits u64"));
}
