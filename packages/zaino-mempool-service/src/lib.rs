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
/// descriptions
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    zaino_status::metric_names! {
        // All off one snapshot per poll (cross-poll reads give a state that never existed)
        gauge MEMPOOL_TRANSACTIONS = "zaino.mempool.transactions" => "Transactions in the published mempool set";
        gauge MEMPOOL_BYTES = "zaino.mempool.bytes" => "Published mempool size: `raw` serialized bytes, `cost` ZIP-401 accounting";
        gauge MEMPOOL_UNADMITTED = "zaino.mempool.unadmitted" => "Transactions known to the validator but refused by Zaino's capacity bound";
        /// `_count` = poll rate = the mempool writer's heartbeat
        histogram MEMPOOL_POLL_SECONDS = "zaino.mempool.poll_seconds" => "Seconds for one mempool poll";
    }

    /// Not a bool: capacity bound / deferred metadata / source error differ.
    /// `zainod` registers it, numbering [`MempoolCompleteness::ALL`] into the help
    pub const MEMPOOL_COMPLETENESS: &str = "zaino.mempool.completeness";

    /// `raw` (serialized) vs `cost` (ZIP-401, what the capacity bound applies to)
    pub const MEMPOOL_BYTES_KIND: &str = "kind";

    /// `zainod` numbers [`MempoolCompleteness::ALL`] into [`MEMPOOL_COMPLETENESS`]'s
    /// help text; re-exported so it needs no direct dep on `zaino-mempool`
    pub use zaino_mempool::snapshot::MempoolCompleteness;
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
