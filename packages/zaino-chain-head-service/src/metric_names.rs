//! Prometheus metric names emitted by the chain head, each with its `# HELP`.

#![allow(missing_docs)] // the `# HELP` beside each name is the description

zaino_status::metric_names! {
    // `reorg_total` exceeds `reorg_depth`'s `_count` when a fork lands below the
    // retained window: the reorg is known to have happened, its depth is not
    counter CHAIN_HEAD_REORG_TOTAL = "zaino.sync.reorg_total" => "Reorganizations: tip changes where the previous tip stopped being canonical";
    histogram CHAIN_HEAD_REORG_DEPTH = "zaino.sync.reorg_depth" => "Blocks rewritten by a reorganization: previous tip height minus the fork point";
}
