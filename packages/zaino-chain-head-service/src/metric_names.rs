//! Prometheus metric names emitted by the chain head, with the `# HELP` `zainod` registers.

#![allow(missing_docs)] // the `# HELP` in the tables below is the description

// `reorg_total` > `reorg_depth`'s `_count` when a fork lands below the retained window (depth unknown)
pub const CHAIN_HEAD_REORG_TOTAL: &str = "zaino.sync.reorg_total";
pub const CHAIN_HEAD_REORG_DEPTH: &str = "zaino.sync.reorg_depth";

#[rustfmt::skip]
pub const COUNTERS: &[(&str, &str)] = &[
    (CHAIN_HEAD_REORG_TOTAL, "Reorganizations: tip changes where the previous tip stopped being canonical"),
];

#[rustfmt::skip]
pub const HISTOGRAMS: &[(&str, &str)] = &[
    (CHAIN_HEAD_REORG_DEPTH, "Blocks rewritten by a reorganization: previous tip height minus the fork point"),
];
