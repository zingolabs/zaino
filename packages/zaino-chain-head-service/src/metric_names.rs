//! Prometheus metric names emitted by the chain head, each with its `# HELP`.

#![allow(missing_docs)] // the `# HELP` beside each name is the description

zaino_status::metric_names! {
    // `reorg_total` is `reorg_depth`'s `_count`, kept separate so an alert can use it
    // without the histogram
    counter CHAIN_HEAD_REORG_TOTAL = "zaino.sync.reorg_total" => "Chain reorganization events observed by the chain head";
    histogram CHAIN_HEAD_REORG_DEPTH = "zaino.sync.reorg_depth" => "Reorganization depth in blocks; 0 for a same-height tip swap";
}
