//! Prometheus metric names emitted by the chain head, with the `# HELP` `zainod` registers.

#![allow(missing_docs)] // the `# HELP` in the tables below is the description

// `reorg_total` > `reorg_depth`'s `_count` when a fork lands below the retained window (depth unknown)
pub const CHAIN_HEAD_REORG_TOTAL: &str = "zaino.sync.reorg_total";
pub const CHAIN_HEAD_REORG_DEPTH: &str = "zaino.sync.reorg_depth";
// Sync lag = CHAIN_TIP_HEIGHT - SYNC_FINALIZED_HEIGHT, consumer-derived
pub const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";
pub const SYNC_CONSECUTIVE_FAILURES: &str = "zaino.sync.consecutive_failures";
pub const SYNC_BACKOFF_SECONDS: &str = "zaino.sync.backoff_seconds";

#[rustfmt::skip]
pub const COUNTERS: &[(&str, &str)] = &[
    (CHAIN_HEAD_REORG_TOTAL, "Reorganizations: tip changes where the previous tip stopped being canonical"),
];

#[rustfmt::skip]
pub const GAUGES: &[(&str, &str)] = &[
    (CHAIN_TIP_HEIGHT, "Latest chain tip height reported by the source"),
    (SYNC_CONSECUTIVE_FAILURES, "Consecutive failed sync iterations; 0 when healthy"),
    (SYNC_BACKOFF_SECONDS, "Current sync-loop retry backoff in seconds; 0 when healthy"),
];

#[rustfmt::skip]
pub const HISTOGRAMS: &[(&str, &str)] = &[
    (CHAIN_HEAD_REORG_DEPTH, "Blocks rewritten by a reorganization: previous tip height minus the fork point"),
];
