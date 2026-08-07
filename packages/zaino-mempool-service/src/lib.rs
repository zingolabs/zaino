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
