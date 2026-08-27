//! Prometheus metric names emitted by this crate.
//!
//! The single source of truth, shared with `zainod`'s `describe_*`
//! registrations — which carry the descriptions, so a name and its description
//! cannot drift apart across crates.
//!
//! The strings are unchanged from when these were emitted by the non-finalised
//! state inside `zaino-state`. They keep the `zaino.sync.` prefix rather than
//! moving to `zaino.chain_head.` deliberately: renaming would silently break
//! every existing dashboard and alert for no gain in what is measured.

/// Total chain reorganisations observed by the chain head.
pub const CHAIN_HEAD_REORG_TOTAL: &str = "zaino.sync.reorg_total";

/// Depth in blocks of each reorganisation; `0` for a same-height tip swap.
pub const CHAIN_HEAD_REORG_DEPTH: &str = "zaino.sync.reorg_depth";

/// Histograms this crate emits, so `zainod` can prove its bucket-bound table
/// covers exactly what the workspace records.
///
/// Declared here rather than in the table: the emitting crate is the only one
/// that knows what it emits, and a histogram published with no declared bounds
/// silently gets the exporter's defaults.
pub const HISTOGRAM_METRICS: [&str; 1] = [CHAIN_HEAD_REORG_DEPTH];
