//! `zaino-mempool-service` — concrete adapters/implementations of the mempool ports.
//!
//! This crate is the hexagonal *adapter* layer for the mempool subsystem. It
//! supplies the runtime machinery that drives the ports defined in
//! [`zaino-mempool`](zaino_mempool):
//!
//! - [`CoherenceService`] (feature `tip_aware_mempool`) — the tip-aware coherence
//!   layer: consumes a [`zaino_mempool::Mempool`] core and an
//!   [`zaino_mempool::NfsEpochObserver`] and publishes the coherent view + stream
//!   that combined ChainIndex reads consult.
//! - [`MempoolService`] — the tip-agnostic core: a polling writer that mirrors the
//!   validator's mempool as a bounded, never-frozen read model, tagged with the
//!   validator tip each set was fetched at. It implements
//!   [`zaino_mempool::Mempool`] via its [`MempoolSubscriber`] read handle.
//!
//! Dependencies point inward: this crate depends on `zaino-mempool` (the ports +
//! foundational types); `zaino-mempool` never names anything here.

/// Prometheus metric names emitted by this crate; the single source of truth
/// shared with `zainod`'s `describe_*` registrations, which carry the
/// descriptions.
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    // Published set shape. All four off one snapshot per poll, so mutually
    // consistent — cross-poll reads give an average entry size that never existed
    pub const MEMPOOL_TRANSACTIONS: &str = "zaino.mempool.transactions";
    pub const MEMPOOL_BYTES: &str = "zaino.mempool.bytes";
    pub const MEMPOOL_UNADMITTED: &str = "zaino.mempool.unadmitted";

    /// Set completeness, as a `MempoolCompleteness` discriminant.
    ///
    /// - Only correctness fact here, not a size: non-zero = a known partial view
    /// - Not a bool — capacity bound / deferred metadata / source error differ
    pub const MEMPOOL_COMPLETENESS: &str = "zaino.mempool.completeness";

    /// One poll: tip, txid diff, fetch+admit additions, publish.
    ///
    /// - `_count` = poll rate = the writer's heartbeat (cf. `sync.iterations_total`)
    pub const MEMPOOL_POLL_SECONDS: &str = "zaino.mempool.poll_seconds";

    /// [`MEMPOOL_BYTES`] accounting: `raw` (serialized) or `cost` (ZIP-401, what
    /// the capacity bound applies to). One labelled metric — the ratio is the read.
    pub const MEMPOOL_BYTES_KIND: &str = "kind";

    /// Every value [`MEMPOOL_BYTES_KIND`] takes, for pre-creation.
    pub const MEMPOOL_BYTES_KINDS: [&str; 2] = ["raw", "cost"];

    /// Every histogram above, for `zainod` to check its bucket table against.
    /// Rationale: [`zaino_state::metric_names::HISTOGRAM_METRICS`].
    pub const HISTOGRAM_METRICS: [&str; 1] = [MEMPOOL_POLL_SECONDS];

    /// [`MEMPOOL_COMPLETENESS`] names in discriminant order; `zainod` renders it
    /// into help text rather than a dashboard retyping (and drifting from) it.
    pub const MEMPOOL_COMPLETENESS_VALUES: [&str; 4] = [
        "complete",
        "incomplete-capacity-limited",
        "incomplete-pending-metadata",
        "incomplete-source-error",
    ];
}

pub mod service;
pub mod subscriber;

#[cfg(feature = "tip_aware_mempool")]
pub mod coherence;

#[cfg(test)]
mod tests;

pub use service::MempoolService;
pub use subscriber::{MempoolFilterError, MempoolInfo, MempoolSubscriber, TxIdExcludeSuffix};

#[cfg(feature = "tip_aware_mempool")]
pub use coherence::{CoherenceService, CoherentSubscriber};

#[cfg(all(test, feature = "prometheus"))]
mod metric_tests {
    use crate::metric_names::MEMPOOL_COMPLETENESS_VALUES;
    use zaino_mempool::MempoolCompleteness;

    /// - Gauge publishes `MempoolCompleteness as u8`, legend =
    ///   [`MEMPOOL_COMPLETENESS_VALUES`]
    /// - `#[repr(u8)]` + explicit discriminants stop a mid-enum insert renumbering
    /// - Left to catch here: a discriminant edited, or a variant appended without
    ///   extending the legend
    #[test]
    fn completeness_discriminants_match_their_legend_positions() {
        for (completeness, expected) in [
            (MempoolCompleteness::Complete, "complete"),
            (
                MempoolCompleteness::IncompleteCapacityLimited,
                "incomplete-capacity-limited",
            ),
            (
                MempoolCompleteness::IncompletePendingMetadata,
                "incomplete-pending-metadata",
            ),
            (
                MempoolCompleteness::IncompleteSourceError,
                "incomplete-source-error",
            ),
        ] {
            let discriminant = completeness as usize;
            assert_eq!(
                MEMPOOL_COMPLETENESS_VALUES.get(discriminant),
                Some(&expected),
                "{completeness:?} publishes as {discriminant}, but the legend calls \
                 that position {:?}",
                MEMPOOL_COMPLETENESS_VALUES.get(discriminant),
            );
        }
        assert_eq!(
            MEMPOOL_COMPLETENESS_VALUES.len(),
            4,
            "a MempoolCompleteness variant was added or removed without updating \
             MEMPOOL_COMPLETENESS_VALUES"
        );
    }
}
