//! LSM metrics, labelled `index` = the index's name and `set` = the map (segment set) in it
//!
//! - write amplification = `merge_rows_total / batch_rows_total`

use std::time::Duration;

use super::{writer::Merged, SegmentMeta};

mod names {
    pub(super) const SEGMENTS: &str = "zaino.lsm.segments";
    pub(super) const MERGING: &str = "zaino.lsm.merging";
    pub(super) const STALL_AT: &str = "zaino.lsm.stall_at";
    pub(super) const BATCH_ROWS_TOTAL: &str = "zaino.lsm.batch_rows_total";
    pub(super) const MERGE_ROWS_TOTAL: &str = "zaino.lsm.merge_rows_total";
    pub(super) const MERGE_BYTES_TOTAL: &str = "zaino.lsm.merge_bytes_total";
    pub(super) const MERGE_CANCELLED_TOTAL: &str = "zaino.lsm.merge_cancelled_total";
    pub(super) const MERGE_DURATION_SECONDS: &str = "zaino.lsm.merge_duration_seconds";
    pub(super) const STALL_DURATION_SECONDS: &str = "zaino.lsm.stall_duration_seconds";
    pub(super) const LEFTOVER_BYTES: &str = "zaino.lsm.leftover_bytes";
    pub(super) const MERGE_LAUNCH_FAILED_TOTAL: &str = "zaino.lsm.merge_launch_failed_total";
    pub(super) const UNLINKED_MAPPED_BYTES: &str = "zaino.lsm.unlinked_mapped_bytes";
}

/// `(metric, bucket edges)` for the exporter (ms tier-0 merges → tens of minutes at the top)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        names::MERGE_DURATION_SECONDS,
        &[
            1e-2, 5e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1200.0, 3600.0,
        ],
    ),
    (
        names::STALL_DURATION_SECONDS,
        &[1e-3, 1e-2, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0],
    ),
];

/// `# HELP` registrations for every metric this module emits
pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_gauge!(
        names::SEGMENTS,
        "Committed segments a read searches, by index, set and size tier (⌊log_fanout rows⌋)"
    );
    describe_gauge!(
        names::MERGING,
        "1 = a background merge of this size tier is running, by index, set and tier"
    );
    describe_gauge!(
        names::STALL_AT,
        "Segments a merging tier may list before a commit waits on its merge, by index and set"
    );
    describe_counter!(
        names::BATCH_ROWS_TOTAL,
        "Rows written as new segments by commits, by index and set"
    );
    describe_counter!(
        names::MERGE_ROWS_TOTAL,
        "Rows rewritten by background merges a commit adopted, by index and set"
    );
    describe_counter!(
        names::MERGE_BYTES_TOTAL,
        Unit::Bytes,
        "Segment bytes written by background merges a commit adopted, by index and set"
    );
    describe_counter!(
        names::MERGE_CANCELLED_TOTAL,
        "Key value + tombstone pairs dropped by background merges a commit adopted, by index and \
         set"
    );
    describe_histogram!(
        names::MERGE_DURATION_SECONDS,
        Unit::Seconds,
        "One adopted background merge, read through sealed and linked, by index, set and input tier"
    );
    describe_histogram!(
        names::STALL_DURATION_SECONDS,
        Unit::Seconds,
        "Commit time spent waiting on a merge that fell two windows behind, by index and set"
    );
    describe_gauge!(
        names::LEFTOVER_BYTES,
        Unit::Bytes,
        "Bytes of files no manifest lists (merge-replaced segments, a failed merge's partial \
         files) whose removal keeps failing, by index and set"
    );
    describe_counter!(
        names::MERGE_LAUNCH_FAILED_TOTAL,
        "Background merges that failed to start, by index and set"
    );
    describe_gauge!(
        names::UNLINKED_MAPPED_BYTES,
        Unit::Bytes,
        "Deleted segment bytes still mapped by a reader (disk freed when it drops), by index and \
         set"
    );
}

/// One map's metric labels: its index and its set
#[derive(Debug, Clone)]
pub(crate) struct Labels {
    index: String,
    set: String,
}

impl Labels {
    pub(crate) fn new(index: &str, set: &str) -> Self {
        Self {
            index: index.to_owned(),
            set: set.to_owned(),
        }
    }

    /// The map's name
    pub(crate) fn set(&self) -> &str {
        &self.set
    }

    fn of(&self) -> [(&'static str, String); 2] {
        [("index", self.index.clone()), ("set", self.set.clone())]
    }

    fn with_tier(&self, tier: impl ToString) -> [(&'static str, String); 3] {
        let [index, set] = self.of();
        [index, set, ("tier", tier.to_string())]
    }
}

/// Per tier `0..shape.len()`: `(segments listed, merge running)`
///
/// - every tier re-sent, zeros included (emptied tier never keeps a stale count)
pub(super) fn shape(labels: &Labels, shape: &[(usize, bool)]) {
    for (tier, &(segments, merging)) in shape.iter().enumerate() {
        let labels = labels.with_tier(tier);
        metrics::gauge!(names::SEGMENTS, &labels).set(segments as f64);
        metrics::gauge!(names::MERGING, &labels).set(if merging { 1.0 } else { 0.0 });
    }
}

pub(super) fn stall_at(labels: &Labels, segments: usize) {
    metrics::gauge!(names::STALL_AT, &labels.of()).set(segments as f64);
}

pub(super) fn batched(labels: &Labels, segment: &SegmentMeta) {
    metrics::counter!(names::BATCH_ROWS_TOTAL, &labels.of()).increment(segment.records);
}

/// A merge a commit adopted; `tier` = the inputs' tier (output lands one above)
pub(super) fn merged(labels: &Labels, tier: u32, merged: &Merged, took: Duration) {
    let (rows, bytes) = merged
        .output
        .map_or((0, 0), |output| (output.records, output.sealed.len));
    let of = labels.of();
    metrics::counter!(names::MERGE_ROWS_TOTAL, &of).increment(rows);
    metrics::counter!(names::MERGE_BYTES_TOTAL, &of).increment(bytes);
    metrics::counter!(names::MERGE_CANCELLED_TOTAL, &of).increment(merged.cancelled);
    metrics::histogram!(names::MERGE_DURATION_SECONDS, &labels.with_tier(tier)).record(took);
}

pub(super) fn stalled(labels: &Labels, took: Duration) {
    metrics::histogram!(names::STALL_DURATION_SECONDS, &labels.of()).record(took);
}

/// Bytes of unlisted files whose removal keeps failing (retried every write)
pub(super) fn leftover(labels: &Labels, bytes: u64) {
    metrics::gauge!(names::LEFTOVER_BYTES, &labels.of()).set(bytes as f64);
}

/// Deleted segment bytes still mapped: raised at unlink, lowered as the last reader drops
pub(super) fn unlinked(labels: &Labels) -> metrics::Gauge {
    metrics::gauge!(names::UNLINKED_MAPPED_BYTES, &labels.of())
}

pub(super) fn launch_failed(labels: &Labels) {
    metrics::counter!(names::MERGE_LAUNCH_FAILED_TOTAL, &labels.of()).increment(1);
}
