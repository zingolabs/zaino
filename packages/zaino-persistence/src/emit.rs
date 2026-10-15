//! Store metrics, labelled `index` = the store's index kind (LSM sets: `lsm/emit.rs`)

use std::time::Duration;

mod metric_names {
    pub(super) const COMMIT_SECONDS: &str = "zaino.store.commit_seconds";
    pub(super) const COMMIT_BYTES_TOTAL: &str = "zaino.store.commit_bytes_total";
}

/// `(metric, bucket edges)` for the exporter, every histogram this crate emits
///
/// - commits: ms at the tip → minutes on a stalled disk
/// - merges: ms at tier 0 → an hour at the top
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        metric_names::COMMIT_SECONDS,
        &[1e-3, 5e-3, 1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0],
    ),
    (
        crate::lsm::metric_names::MERGE_DURATION_SECONDS,
        &[1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1200.0, 3600.0],
    ),
    (
        crate::lsm::metric_names::STALL_DURATION_SECONDS,
        &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0],
    ),
];

/// `# HELP` registrations for every metric this crate emits (store + LSM)
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_histogram, Unit};

    describe_histogram!(
        metric_names::COMMIT_SECONDS,
        Unit::Seconds,
        "One durable commit: every table written + synced, then the manifest, by index"
    );
    describe_counter!(
        metric_names::COMMIT_BYTES_TOTAL,
        Unit::Bytes,
        "Record + row bytes made durable by commits (before encoding), by index"
    );
    crate::lsm::describe_metrics();
}

pub(crate) fn committed(index: &'static str, bytes: usize, took: Duration) {
    metrics::histogram!(metric_names::COMMIT_SECONDS, "index" => index).record(took.as_secs_f64());
    metrics::counter!(metric_names::COMMIT_BYTES_TOTAL, "index" => index).increment(bytes as u64);
}
