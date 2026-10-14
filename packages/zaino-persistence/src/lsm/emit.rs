//! LSM metrics (labelled `set` = the segment set's directory name) + operator log lines
//!
//! - write amplification = `merge_rows_total / batch_rows_total`
//! - every merge = DEBUG: `Compacting segments` at launch, `Compacted segments` once the manifest
//!   listing its output is durable (routine: bulk sync lands several a second)
//! - commit waiting on a merge = WARN (go-ethereum "Database compacting, degraded performance")

use std::{fmt, time::Duration};

use tracing::{debug, warn};

use super::{log::Landed, writer::Merged, SegmentMeta};

pub(crate) mod metric_names {
    pub(super) const SEGMENTS: &str = "zaino.lsm.segments";
    pub(super) const MERGING: &str = "zaino.lsm.merging";
    pub(super) const STALL_AT: &str = "zaino.lsm.stall_at";
    pub(super) const BATCH_ROWS_TOTAL: &str = "zaino.lsm.batch_rows_total";
    pub(super) const MERGE_ROWS_TOTAL: &str = "zaino.lsm.merge_rows_total";
    pub(super) const MERGE_BYTES_TOTAL: &str = "zaino.lsm.merge_bytes_total";
    pub(super) const MERGE_CANCELLED_TOTAL: &str = "zaino.lsm.merge_cancelled_total";
    pub(crate) const MERGE_DURATION_SECONDS: &str = "zaino.lsm.merge_duration_seconds";
    pub(crate) const STALL_DURATION_SECONDS: &str = "zaino.lsm.stall_duration_seconds";
}

/// `# HELP` registrations for every metric this module emits
pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_gauge!(
        metric_names::SEGMENTS,
        "Committed segments a read searches, by set and size tier (⌊log_fanout rows⌋)"
    );
    describe_gauge!(
        metric_names::MERGING,
        "1 = a background merge of this size tier is running, by set and tier"
    );
    describe_gauge!(
        metric_names::STALL_AT,
        "Segments a merging tier may list before a commit waits on its merge, by set"
    );
    describe_counter!(
        metric_names::BATCH_ROWS_TOTAL,
        "Rows written as new segments by commits, by set"
    );
    describe_counter!(
        metric_names::MERGE_ROWS_TOTAL,
        "Rows rewritten by finished background merges, by set"
    );
    describe_counter!(
        metric_names::MERGE_BYTES_TOTAL,
        Unit::Bytes,
        "Segment bytes written by finished background merges, by set"
    );
    describe_counter!(
        metric_names::MERGE_CANCELLED_TOTAL,
        "Key value + tombstone pairs dropped by finished background merges, by set"
    );
    describe_histogram!(
        metric_names::MERGE_DURATION_SECONDS,
        Unit::Seconds,
        "One background merge, read through sealed and linked, by set and input tier"
    );
    describe_histogram!(
        metric_names::STALL_DURATION_SECONDS,
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
        metrics::gauge!(metric_names::SEGMENTS, &labels).set(segments as f64);
        metrics::gauge!(metric_names::MERGING, &labels).set(if merging { 1.0 } else { 0.0 });
    }
}

/// Every per-set series registered at open (absent on /metrics = not built, never "nothing yet")
///
/// - merge histogram registered at tier 0 (first tier to merge)
pub(super) fn opened(set: &str, stall_at: usize) {
    let set = set.to_owned();
    metrics::gauge!(metric_names::STALL_AT, "set" => set.clone()).set(stall_at as f64);
    for counter in [
        metric_names::BATCH_ROWS_TOTAL,
        metric_names::MERGE_ROWS_TOTAL,
        metric_names::MERGE_BYTES_TOTAL,
        metric_names::MERGE_CANCELLED_TOTAL,
    ] {
        metrics::counter!(counter, "set" => set.clone()).increment(0);
    }
    let _ = metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, "set" => set.clone(), "tier" => "0");
    let _ = metrics::histogram!(metric_names::STALL_DURATION_SECONDS, "set" => set);
}

pub(super) fn batched(set: &str, segment: &SegmentMeta) {
    metrics::counter!(metric_names::BATCH_ROWS_TOTAL, "set" => set.to_owned())
        .increment(segment.records);
}

/// `tier` = the inputs' tier (output lands one above)
pub(super) fn merged(set: &str, tier: u32, merged: &Merged, took: Duration) {
    let set = set.to_owned();
    let (rows, bytes) = merged.output.map_or((0, 0), |output| (output.records, output.sealed.len));
    metrics::counter!(metric_names::MERGE_ROWS_TOTAL, "set" => set.clone()).increment(rows);
    metrics::counter!(metric_names::MERGE_BYTES_TOTAL, "set" => set.clone()).increment(bytes);
    metrics::counter!(metric_names::MERGE_CANCELLED_TOTAL, "set" => set.clone())
        .increment(merged.cancelled);
    metrics::histogram!(metric_names::MERGE_DURATION_SECONDS, "set" => set, "tier" => tier.to_string())
        .record(took);
}

/// `tiers` = merging tiers the commit joined (each `stall_at` segments deep)
pub(super) fn stalled(set: &str, tiers: &[u32], waited: Duration) {
    metrics::histogram!(metric_names::STALL_DURATION_SECONDS, "set" => set.to_owned())
        .record(waited);
    warn!(set, tiers = ?tiers, waited = %Elapsed(waited), "Commit waited on compaction");
}

pub(super) fn launched(set: &str, tier: u32, inputs: &[SegmentMeta]) {
    let bytes: u64 = inputs.iter().map(|segment| segment.sealed.len).sum();
    let rows: u64 = inputs.iter().map(|segment| segment.records).sum();
    debug!(set, tier, segments = inputs.len(), rows, size = %Size(bytes), "Compacting segments");
}

pub(super) fn swapped(set: &str, landed: &Landed) {
    let output = landed.merged.output;
    debug!(
        set,
        tier = landed.tier,
        rows = output.map_or(0, |output| output.records),
        cancelled = landed.merged.cancelled,
        size = %Size(output.map_or(0, |output| output.sealed.len)),
        took = %Elapsed(landed.took),
        "Compacted segments"
    );
}

pub(super) fn cancelled(set: &str, merges: usize) {
    debug!(set, merges, "Compaction cancelled at close");
}

/// `412.3MiB` (binary units, no space: one logfmt token)
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.0 as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        match unit {
            0 => write!(f, "{}B", self.0),
            _ => write!(f, "{value:.1}{}", UNITS[unit]),
        }
    }
}

/// `812ms` under a second, `3.2s` from there
struct Elapsed(Duration);

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_millis() {
            ms if ms < 1_000 => write!(f, "{ms}ms"),
            _ => write!(f, "{:.1}s", self.0.as_secs_f64()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations_read_as_one_token() {
        for (bytes, shown) in [
            (0, "0B"),
            (1_023, "1023B"),
            (1_024, "1.0KiB"),
            (432_346_234, "412.3MiB"),
            (3 << 40, "3.0TiB"),
            (u64::MAX, "16777216.0TiB"),
        ] {
            assert_eq!(Size(bytes).to_string(), shown, "{bytes}");
        }
        for (ms, shown) in [(0, "0ms"), (812, "812ms"), (1_000, "1.0s"), (3_249, "3.2s")] {
            assert_eq!(Elapsed(Duration::from_millis(ms)).to_string(), shown, "{ms}");
        }
    }
}
