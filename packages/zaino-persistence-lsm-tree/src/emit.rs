//! Store metrics, labelled `index` = the backend's index name (LSM sets: `crate::lsm`)

use std::time::Duration;

mod names {
    pub(super) const COMMIT_SECONDS: &str = "zaino.store.commit_seconds";
    pub(super) const COMMIT_BYTES_TOTAL: &str = "zaino.store.commit_bytes_total";
    pub(super) const SNAPSHOTS_LIVE: &str = "zaino.store.snapshots_live";
    pub(super) const SNAPSHOT_HEAP_BYTES: &str = "zaino.store.snapshot_heap_bytes";
}

/// `(metric, bucket edges)` for the exporter (ms commits at the tip → minutes on a stalled disk);
/// the LSM's are [`LSM_METRIC_BUCKETS`](crate::LSM_METRIC_BUCKETS)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[(
    names::COMMIT_SECONDS,
    &[
        1e-3, 5e-3, 1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0,
    ],
)];

/// `# HELP` registrations for every metric this crate emits (store + LSM)
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_histogram!(
        names::COMMIT_SECONDS,
        Unit::Seconds,
        "One durable commit: every namespace written + synced, then the manifest, by index"
    );
    describe_counter!(
        names::COMMIT_BYTES_TOTAL,
        Unit::Bytes,
        "Record + row bytes made durable by commits (before encoding), by index"
    );
    describe_gauge!(
        names::SNAPSHOTS_LIVE,
        "Published read states alive (the current one + those readers and scans still hold), by \
         index"
    );
    describe_gauge!(
        names::SNAPSHOT_HEAP_BYTES,
        Unit::Bytes,
        "Estimated buffer heap of the live read states, an upper bound (they share most nodes), \
         by index"
    );
    crate::lsm::describe_metrics();
}

pub(crate) fn committed(index: &str, bytes: usize, took: Duration) {
    let index = index.to_owned();
    metrics::histogram!(names::COMMIT_SECONDS, "index" => index.clone()).record(took.as_secs_f64());
    metrics::counter!(names::COMMIT_BYTES_TOTAL, "index" => index).increment(bytes as u64);
}

/// `index`'s live-states and their heap gauges, moved by each state's creation and drop
pub(crate) fn versions(index: &str) -> (metrics::Gauge, metrics::Gauge) {
    let index = index.to_owned();
    (
        metrics::gauge!(names::SNAPSHOTS_LIVE, "index" => index.clone()),
        metrics::gauge!(names::SNAPSHOT_HEAP_BYTES, "index" => index),
    )
}

/// `emit` run in a `drop` so a panicking recorder can't unwind out of it (the metric is lost;
/// unwinding out of a drop during a panic aborts the process)
pub(crate) fn in_drop(emit: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(emit));
}
