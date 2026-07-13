//! A mempool-fetching, chain-fetching and transaction submission service that uses zcashd's JsonRPC interface.
//!
//! Usable as a backwards-compatible, legacy option.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod jsonrpsee;
pub mod utils;

/// Prometheus metric names emitted by this crate; the single source of truth shared with `zainod`'s `describe_*` registrations (which carry the descriptions).
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    pub const RPC_OUTBOUND_REQUESTS_TOTAL: &str = "zaino.rpc.outbound.requests_total";
    pub const RPC_OUTBOUND_REQUEST_DURATION_SECONDS: &str =
        "zaino.rpc.outbound.request_duration_seconds";
    pub const RPC_OUTBOUND_ERRORS_TOTAL: &str = "zaino.rpc.outbound.errors_total";
    pub const RPC_OUTBOUND_RETRIES_TOTAL: &str = "zaino.rpc.outbound.retries_total";
}
