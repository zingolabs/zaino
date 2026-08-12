//! A mempool-fetching, chain-fetching and transaction submission service that uses zcashd's JsonRPC interface.
//!
//! Usable as a backwards-compatible, legacy option.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod jsonrpsee;
pub mod utils;

/// Prometheus metric names emitted by this crate
#[cfg(feature = "prometheus")]
pub mod metric_names {
    pub const RPC_OUTBOUND_RETRIES_TOTAL: &str = "zaino.rpc.outbound.retries_total";
}
