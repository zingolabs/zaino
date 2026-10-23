//! Operator log lines for background compaction
//!
//! - Every merge = DEBUG: `Compacting segments` at launch, `Compacted segments` once the
//!   manifest listing its output is durable (routine: bulk sync lands several a second)
//! - Commit waiting on a merge = WARN (go-ethereum "Database compacting, degraded performance")

use std::{fmt, io, path::Path, time::Duration};

use tracing::{debug, error, info, warn};

use super::{writer::Merged, SegmentError, SegmentMeta};

/// Merge the log swaps in: `tier` → `merged`, `took` = its thread's wall time
#[derive(Debug)]
pub(super) struct Landed {
    pub(super) tier: u32,
    pub(super) merged: Merged,
    pub(super) took: Duration,
}

pub(super) fn launched(set: &str, tier: u32, inputs: &[SegmentMeta]) {
    let bytes = inputs
        .iter()
        .fold(0u64, |sum, segment| sum.saturating_add(segment.sealed.len));
    let rows = inputs
        .iter()
        .fold(0u64, |sum, segment| sum.saturating_add(segment.records));
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

/// `tiers` = merging tiers the commit joined (each `stall_at` segments deep)
pub(super) fn stalled(set: &str, tiers: &[u32], waited: Duration) {
    warn!(set, tiers = ?tiers, waited = %Elapsed(waited), "Commit waited on compaction");
}

pub(super) fn cancelled(set: &str, merges: usize) {
    debug!(set, merges, "Compaction cancelled at close");
}

/// Scope changed since `stale` were written: their filters rebuilt before the map serves
pub(super) fn rescoping(set: &str, stale: &[SegmentMeta]) {
    let bytes = stale
        .iter()
        .fold(0u64, |sum, segment| sum.saturating_add(segment.sealed.len));
    info!(set, segments = stale.len(), size = %Size(bytes), "Rebuilding filters for a changed scope");
}

pub(super) fn removal_retry(set: &str, path: &Path, retried: u32, error: &io::Error) {
    warn!(set, path = %path.display(), retried, %error, "Removing a merged-away file failed; retrying");
}

/// A failed merge's files could not be listed: removed at the next open (none is listed)
pub(super) fn partial_unlisted(set: &str, segment: u32, error: &io::Error) {
    warn!(set, segment, %error, "A failed merge's files could not be listed; the next open removes them");
}

/// Every map's leftovers together; `alert` = at or past `LsmConfig::cleanup_alert_bytes`: an
/// error (the disk may be failing)
pub(crate) fn leftover(files: usize, bytes: u64, error: &str, alert: bool) {
    match alert {
        true => error!(
            files, size = %Size(bytes), error,
            "Merged-away files can't be removed: the disk may be failing"
        ),
        false => warn!(
            files, size = %Size(bytes), error,
            "Merged-away files not removed; retrying at the next write"
        ),
    }
}

pub(super) fn launch_failed(set: &str, error: &SegmentError) {
    warn!(set, %error, "A merge failed to start; the next write returns the error");
}

/// `412.3MiB` (binary units, no space: one logfmt token)
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.0 as f64;
        let mut unit = 0usize;
        while value >= 1024.0 && unit.saturating_add(1) < UNITS.len() {
            value /= 1024.0;
            unit = unit.saturating_add(1);
        }
        match UNITS.get(unit) {
            Some(&"B") | None => write!(f, "{}B", self.0),
            Some(name) => write!(f, "{value:.1}{name}"),
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
            assert_eq!(
                Elapsed(Duration::from_millis(ms)).to_string(),
                shown,
                "{ms}"
            );
        }
    }
}
