//! LSM metrics, labelled `index` only (every map of an index summed: tree_state's per-level maps =
//! one series each) + operator log lines (per map)
//!
//! - write amplification = `merge_bytes_total / zaino_store_commit_bytes_total` (both per index)
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
    pub(super) const MERGE_BYTES_TOTAL: &str = "zaino.lsm.merge_bytes_total";
    pub(super) const MERGE_DURATION_SECONDS: &str = "zaino.lsm.merge_duration_seconds";
    pub(super) const STALL_DURATION_SECONDS: &str = "zaino.lsm.stall_duration_seconds";
}

/// `# HELP` registrations for every metric this module emits
pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_gauge!(
        metric_names::SEGMENTS,
        "Committed segments, summed over the index's maps (a read searches its map's)"
    );
    describe_gauge!(
        metric_names::LIVE_ROWS,
        "Keys the committed segments hold, summed over the index's maps (a `deletes()` map: \
         values minus tombstones)"
    );
    describe_gauge!(metric_names::MERGING, "Background merges running, by index");
    describe_counter!(
        metric_names::MERGE_BYTES_TOTAL,
        Unit::Bytes,
        "Segment bytes written by finished background merges, by index"
    );
    describe_histogram!(
        metric_names::MERGE_DURATION_SECONDS,
        Unit::Seconds,
        "One background merge, read through sealed and linked, by index"
    );
    describe_histogram!(
        metric_names::STALL_DURATION_SECONDS,
        Unit::Seconds,
        "Commit time spent waiting on a merge that fell two windows behind, by index"
    );
}

/// One map's names (`index` = the metric label; `table` = log lines only)
#[derive(Debug, Clone, Copy)]
pub(super) struct Series {
    pub(super) index: &'static str,
    pub(super) table: &'static str,
}

impl Series {
    fn labels(self) -> [(&'static str, &'static str); 1] {
        [("index", self.index)]
    }
}

/// One map's share of its index's gauges, sent as the change since its last report (an index's
/// maps share each series: a `set` would keep only the last map's), withdrawn on drop
///
/// - built at open: every series of the index registered (absent on /metrics = not built, never
///   "nothing yet")
pub(super) struct IndexShare {
    series: Series,
    segments: usize,
    merging: usize,
    live_rows: i64,
}

impl IndexShare {
    pub(super) fn opened(series: Series) -> Self {
        let labels = series.labels();
        metrics::counter!(metric_names::MERGE_BYTES_TOTAL, &labels).increment(0);
        let _ = metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, &labels);
        let _ = metrics::histogram!(metric_names::STALL_DURATION_SECONDS, &labels);
        let share = Self { series, segments: 0, merging: 0, live_rows: 0 };
        share.add(0.0, 0.0, 0.0);
        share
    }

    /// The map's listed segments, its running merges, its live keys
    pub(super) fn report(&mut self, segments: usize, merging: usize, live_rows: i64) {
        self.add(
            segments as f64 - self.segments as f64,
            merging as f64 - self.merging as f64,
            (live_rows - self.live_rows) as f64,
        );
        (self.segments, self.merging, self.live_rows) = (segments, merging, live_rows);
    }

    fn add(&self, segments: f64, merging: f64, live_rows: f64) {
        let labels = self.series.labels();
        metrics::gauge!(metric_names::SEGMENTS, &labels).increment(segments);
        metrics::gauge!(metric_names::MERGING, &labels).increment(merging);
        metrics::gauge!(metric_names::LIVE_ROWS, &labels).increment(live_rows);
    }
}

impl Drop for IndexShare {
    fn drop(&mut self) {
        self.report(0, 0, 0);
    }
}

pub(super) fn merged(series: Series, merged: &Merged, took: Duration) {
    let labels = series.labels();
    let bytes = merged.output.map_or(0, |output| output.sealed.len);
    metrics::counter!(metric_names::MERGE_BYTES_TOTAL, &labels).increment(bytes);
    metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, &labels).record(took);
}

/// `tiers` = merging tiers the commit joined (each [`stall_at`](super::stall_at) segments deep)
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
