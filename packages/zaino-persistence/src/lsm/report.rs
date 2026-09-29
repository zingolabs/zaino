//! Operator log lines for background compaction
//!
//! - Every merge = DEBUG: `Compacting segments` at launch, `Compacted segments` once the
//!   manifest listing its output is durable (routine: bulk sync lands several a second)
//! - Commit waiting on a merge = WARN (go-ethereum "Database compacting, degraded performance")

use std::{fmt, time::Duration};

use tracing::{debug, warn};

use super::SegmentMeta;

/// A merge the log swaps in: `tier` → `output`, `took` = its thread's wall time
#[derive(Debug, Clone, Copy)]
pub(super) struct Landed {
    pub(super) tier: u32,
    pub(super) output: SegmentMeta,
    pub(super) took: Duration,
}

pub(super) fn launched(set: &str, tier: u32, inputs: &[SegmentMeta]) {
    let bytes: u64 = inputs.iter().map(|segment| segment.sealed.len).sum();
    let rows: u64 = inputs.iter().map(|segment| segment.records).sum();
    debug!(set, tier, segments = inputs.len(), rows, size = %Size(bytes), "Compacting segments");
}

pub(super) fn swapped(set: &str, landed: &Landed) {
    debug!(
        set,
        tier = landed.tier,
        rows = landed.output.records,
        size = %Size(landed.output.sealed.len),
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

/// `412.3MiB` (binary units, no space: one logfmt token)
pub struct Size(pub u64);

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
