//! LSM metrics (labelled `index` = the store's index kind, `table` = the map's name) + operator
//! log lines
//!
//! - write amplification = `merge_rows_total / batch_rows_total`
//! - every merge = DEBUG: `Compacting segments` at launch, `Compacted segments` once the manifest
//!   listing its output is durable (routine: bulk sync lands several a second)
//! - commit waiting on a merge = WARN (go-ethereum "Database compacting, degraded performance")

use std::time::Duration;

use tracing::{debug, warn};
use zaino_primitives::display::{ByteSize, Human};

use super::{segment_set::Landed, writer::Merged, SegmentMeta};

mod metric_names {
    pub(super) const SEGMENTS: &str = "zaino.lsm.segments";
    pub(super) const LIVE_ROWS: &str = "zaino.lsm.live_rows";
    pub(super) const MERGING: &str = "zaino.lsm.merging";
    pub(super) const STALL_AT: &str = "zaino.lsm.stall_at";
    pub(super) const BATCH_ROWS_TOTAL: &str = "zaino.lsm.batch_rows_total";
    pub(super) const MERGE_ROWS_TOTAL: &str = "zaino.lsm.merge_rows_total";
    pub(super) const MERGE_BYTES_TOTAL: &str = "zaino.lsm.merge_bytes_total";
    pub(super) const MERGE_CANCELLED_TOTAL: &str = "zaino.lsm.merge_cancelled_total";
    pub(super) const MERGE_DURATION_SECONDS: &str = "zaino.lsm.merge_duration_seconds";
    pub(super) const STALL_DURATION_SECONDS: &str = "zaino.lsm.stall_duration_seconds";
}

/// `# HELP` registrations for every metric this module emits
pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_gauge!(
        metric_names::SEGMENTS,
        "Committed segments a read searches, by index, table and size tier (⌊log_fanout rows⌋)"
    );
    describe_gauge!(
        metric_names::LIVE_ROWS,
        "Keys the committed segments hold, by index and table (a `deletes()` table: values minus \
         tombstones, each cancelling one older value)"
    );
    describe_gauge!(
        metric_names::MERGING,
        "1 = a background merge of this size tier is running, by index, table and tier"
    );
    describe_gauge!(
        metric_names::STALL_AT,
        "Segments a merging tier may list before a commit waits on its merge, by index and table"
    );
    describe_counter!(
        metric_names::BATCH_ROWS_TOTAL,
        "Rows written as new segments by commits, by index and table"
    );
    describe_counter!(
        metric_names::MERGE_ROWS_TOTAL,
        "Rows rewritten by finished background merges, by index and table"
    );
    describe_counter!(
        metric_names::MERGE_BYTES_TOTAL,
        Unit::Bytes,
        "Segment bytes written by finished background merges, by index and table"
    );
    describe_counter!(
        metric_names::MERGE_CANCELLED_TOTAL,
        "Key value + tombstone pairs dropped by finished background merges, by index and table"
    );
    describe_histogram!(
        metric_names::MERGE_DURATION_SECONDS,
        Unit::Seconds,
        "One background merge, read through sealed and linked, by index, table and input tier"
    );
    describe_histogram!(
        metric_names::STALL_DURATION_SECONDS,
        Unit::Seconds,
        "Commit time spent waiting on a merge that fell two windows behind, by index and table"
    );
}

/// One map's label values (table names repeat across indexes: `index` keeps series apart)
#[derive(Debug, Clone, Copy)]
pub(super) struct Series {
    pub(super) index: &'static str,
    pub(super) table: &'static str,
}

impl Series {
    fn labels(self) -> [(&'static str, String); 2] {
        [("index", self.index.to_owned()), ("table", self.table.to_owned())]
    }

    fn tier_labels(self, tier: impl ToString) -> [(&'static str, String); 3] {
        let [index, table] = self.labels();
        [index, table, ("tier", tier.to_string())]
    }
}

/// Per tier `0..shape.len()`: `(segments listed, merge running)`, then the map's live keys
///
/// - every tier re-sent, zeros included (emptied tier never keeps a stale count)
pub(super) fn shape(series: Series, shape: &[(usize, bool)], live_rows: i64) {
    metrics::gauge!(metric_names::LIVE_ROWS, &series.labels()).set(live_rows as f64);
    for (tier, &(segments, merging)) in shape.iter().enumerate() {
        let labels = series.tier_labels(tier);
        metrics::gauge!(metric_names::SEGMENTS, &labels).set(segments as f64);
        metrics::gauge!(metric_names::MERGING, &labels).set(if merging { 1.0 } else { 0.0 });
    }
}

/// Every per-map series registered at open (absent on /metrics = not built, never "nothing yet")
///
/// - merge histogram registered at tier 0 (first tier to merge)
pub(super) fn opened(series: Series, stall_at: usize) {
    let labels = series.labels();
    metrics::gauge!(metric_names::STALL_AT, &labels).set(stall_at as f64);
    for counter in [
        metric_names::BATCH_ROWS_TOTAL,
        metric_names::MERGE_ROWS_TOTAL,
        metric_names::MERGE_BYTES_TOTAL,
        metric_names::MERGE_CANCELLED_TOTAL,
    ] {
        metrics::counter!(counter, &labels).increment(0);
    }
    let _ = metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, &series.tier_labels(0));
    let _ = metrics::histogram!(metric_names::STALL_DURATION_SECONDS, &labels);
}

pub(super) fn batched(series: Series, segment: &SegmentMeta) {
    metrics::counter!(metric_names::BATCH_ROWS_TOTAL, &series.labels()).increment(segment.records);
}

/// `tier` = the inputs' tier (output lands one above)
pub(super) fn merged(series: Series, tier: u32, merged: &Merged, took: Duration) {
    let labels = series.labels();
    let (rows, bytes) = merged.output.map_or((0, 0), |output| (output.records, output.sealed.len));
    metrics::counter!(metric_names::MERGE_ROWS_TOTAL, &labels).increment(rows);
    metrics::counter!(metric_names::MERGE_BYTES_TOTAL, &labels).increment(bytes);
    metrics::counter!(metric_names::MERGE_CANCELLED_TOTAL, &labels).increment(merged.cancelled);
    metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, &series.tier_labels(tier))
        .record(took);
}

/// `tiers` = merging tiers the commit joined (each `stall_at` segments deep)
pub(super) fn stalled(series: Series, tiers: &[u32], waited: Duration) {
    metrics::histogram!(metric_names::STALL_DURATION_SECONDS, &series.labels()).record(waited);
    let Series { index, table } = series;
    warn!(index, table, tiers = ?tiers, waited = %Human(waited), "Commit waited on compaction");
}

pub(super) fn launched(series: Series, tier: u32, inputs: &[SegmentMeta]) {
    let Series { index, table } = series;
    let bytes: u64 = inputs.iter().map(|segment| segment.sealed.len).sum();
    let rows: u64 = inputs.iter().map(|segment| segment.records).sum();
    let size = ByteSize(bytes);
    debug!(index, table, tier, segments = inputs.len(), rows, size = %size, "Compacting segments");
}

pub(super) fn swapped(series: Series, landed: &Landed) {
    let Series { index, table } = series;
    let output = landed.merged.output;
    debug!(
        index,
        table,
        tier = landed.tier,
        rows = output.map_or(0, |output| output.records),
        cancelled = landed.merged.cancelled,
        size = %ByteSize(output.map_or(0, |output| output.sealed.len)),
        took = %Human(landed.took),
        "Compacted segments"
    );
}

pub(super) fn cancelled(series: Series, merges: usize) {
    let Series { index, table } = series;
    debug!(index, table, merges, "Compaction cancelled at close");
}
