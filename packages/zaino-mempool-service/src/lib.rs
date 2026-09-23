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

/// Prometheus metric names emitted by this crate, with the `# HELP` `zainod` registers
#[allow(missing_docs)] // the `# HELP` in the tables below is the description
pub mod metric_names {
    // All off one snapshot per poll (cross-poll reads give a state that never existed)
    pub const MEMPOOL_TRANSACTIONS: &str = "zaino.mempool.transactions";
    pub const MEMPOOL_BYTES: &str = "zaino.mempool.bytes";
    pub const MEMPOOL_UNADMITTED: &str = "zaino.mempool.unadmitted";
    // `_count` = poll rate = the mempool writer's heartbeat
    pub const MEMPOOL_POLL_SECONDS: &str = "zaino.mempool.poll_seconds";

    /// Label on MEMPOOL_BYTES: `raw` (serialized) vs `cost` (ZIP-401, what the bound applies to)
    pub const MEMPOOL_BYTES_KIND: &str = "kind";

    #[rustfmt::skip]
    pub const GAUGES: &[(&str, &str)] = &[
        (MEMPOOL_TRANSACTIONS, "Transactions in the published mempool set"),
        (MEMPOOL_BYTES, "Published mempool size: `raw` serialized bytes, `cost` ZIP-401 accounting"),
        (MEMPOOL_UNADMITTED, "Transactions known to the validator but refused by Zaino's capacity bound"),
    ];

    #[rustfmt::skip]
    pub const HISTOGRAMS: &[(&str, &str)] = &[
        (MEMPOOL_POLL_SECONDS, "Seconds for one mempool poll"),
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
