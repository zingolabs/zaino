//! NFS event counters (names = ztest's `zainod` families: a rename breaks its sync probes)
//!
//! - State gauges (`zaino_best_tip`, `zaino_fetch_height`) = `zaino-snapshot`'s, at scrape
//! - Fetch counters = the final path's (`zaino-sync`)

const REORGS_TOTAL: &str = "zaino.reorgs_total";

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_counter!(
        REORGS_TOTAL,
        "Served tips replaced by another branch rather than extended"
    );
    // published from boot (unregistered until first reorg = indistinguishable from unemitted)
    metrics::counter!(REORGS_TOTAL).absolute(0);
}

pub(crate) fn reorg() {
    metrics::counter!(REORGS_TOTAL).increment(1);
}
