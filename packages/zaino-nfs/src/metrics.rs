//! NFS event counters (names = ztest's `zainod` families: a rename breaks its sync probes)
//!
//! - State gauges (`zaino_best_tip`, `zaino_fetch_height`) = `zaino-grpc-snapshot`'s, at scrape
//! - Fetch counters = the finalized path's (`zaino-sync`)

use zaino_primitives::metric::Metric;

const REORGS_TOTAL: &str = "zaino.reorgs_total";

/// Every metric this crate emits
pub const METRICS: &[Metric] = &[Metric::counter_from_zero(
    REORGS_TOTAL,
    "Served tips replaced by another branch rather than extended",
)];

pub(crate) fn reorg() {
    metrics::counter!(REORGS_TOTAL).increment(1);
}
