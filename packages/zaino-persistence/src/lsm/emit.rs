//! LSM metrics, labelled `set` = the segment set's directory name
//!
//! - write amplification = `merge_rows_total / batch_rows_total`

use std::time::Duration;

use super::SegmentMeta;

mod names {
    pub(super) const SEGMENTS: &str = "zaino.lsm.segments";
    pub(super) const MERGING: &str = "zaino.lsm.merging";
    pub(super) const STALL_AT: &str = "zaino.lsm.stall_at";
    pub(super) const BATCH_ROWS_TOTAL: &str = "zaino.lsm.batch_rows_total";
    pub(super) const MERGE_ROWS_TOTAL: &str = "zaino.lsm.merge_rows_total";
    pub(super) const MERGE_BYTES_TOTAL: &str = "zaino.lsm.merge_bytes_total";
    pub(super) const MERGE_DURATION_SECONDS: &str = "zaino.lsm.merge_duration_seconds";
    pub(super) const STALL_DURATION_SECONDS: &str = "zaino.lsm.stall_duration_seconds";
}

/// `(metric, bucket edges)` for the exporter (ms tier-0 merges → tens of minutes at the top)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        names::MERGE_DURATION_SECONDS,
        &[1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1200.0, 3600.0],
    ),
    (names::STALL_DURATION_SECONDS, &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0]),
];

/// `# HELP` registrations for every metric this module emits
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_gauge!(
        names::SEGMENTS,
        "Committed segments a read searches, by set and size tier (⌊log_fanout rows⌋)"
    );
    describe_gauge!(
        names::MERGING,
        "1 = a background merge of this size tier is running, by set and tier"
    );
    describe_gauge!(
        names::STALL_AT,
        "Segments a merging tier may list before a commit waits on its merge, by set"
    );
    describe_counter!(names::BATCH_ROWS_TOTAL, "Rows written as new segments by commits, by set");
    describe_counter!(
        names::MERGE_ROWS_TOTAL,
        "Rows rewritten by finished background merges, by set"
    );
    describe_counter!(
        names::MERGE_BYTES_TOTAL,
        Unit::Bytes,
        "Segment bytes written by finished background merges, by set"
    );
    describe_histogram!(
        names::MERGE_DURATION_SECONDS,
        Unit::Seconds,
        "One background merge, read through sealed and linked, by set and input tier"
    );
    describe_histogram!(
        names::STALL_DURATION_SECONDS,
        Unit::Seconds,
        "Commit time spent waiting on a merge that fell two windows behind, by set"
    );
}

/// Per tier `0..shape.len()`: `(segments listed, merge running)`
///
/// - every tier re-sent, zeros included (emptied tier never keeps a stale count)
pub(super) fn shape(set: &str, shape: &[(usize, bool)]) {
    for (tier, &(segments, merging)) in shape.iter().enumerate() {
        let labels = [("set", set.to_owned()), ("tier", tier.to_string())];
        metrics::gauge!(names::SEGMENTS, &labels).set(segments as f64);
        metrics::gauge!(names::MERGING, &labels).set(if merging { 1.0 } else { 0.0 });
    }
}

pub(super) fn stall_at(set: &str, segments: usize) {
    metrics::gauge!(names::STALL_AT, "set" => set.to_owned()).set(segments as f64);
}

pub(super) fn batched(set: &str, segment: &SegmentMeta) {
    metrics::counter!(names::BATCH_ROWS_TOTAL, "set" => set.to_owned()).increment(segment.records);
}

/// `tier` = the inputs' tier (output lands one above)
pub(super) fn merged(set: &str, tier: u32, segment: &SegmentMeta, took: Duration) {
    {
        let set = set.to_owned();
        metrics::counter!(names::MERGE_ROWS_TOTAL, "set" => set.clone()).increment(segment.records);
        metrics::counter!(names::MERGE_BYTES_TOTAL, "set" => set.clone())
            .increment(segment.sealed.len);
        metrics::histogram!(names::MERGE_DURATION_SECONDS, "set" => set, "tier" => tier.to_string())
            .record(took);
    }
}

pub(super) fn stalled(set: &str, took: Duration) {
    metrics::histogram!(names::STALL_DURATION_SECONDS, "set" => set.to_owned()).record(took);
}
