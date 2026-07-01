//! Holds gRPC and JSON RPC servers capable of servicing clients over TCP.
//!
//! - server::ingestor has been built so that other ingestors may be added that use different transport protocols (Nym, TOR).
//!
//! Also holds rust implementations of the LightWallet gRPC Service (CompactTxStreamerServer).

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod rpc;
pub mod server;

/// Prometheus metric names emitted by this crate; the single source of truth shared with `zainod`'s `describe_*` registrations (which carry the descriptions).
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    pub const GRPC_REQUESTS_TOTAL: &str = "zaino.grpc.requests_total";
    pub const GRPC_REQUEST_DURATION_SECONDS: &str = "zaino.grpc.request_duration_seconds";
    pub const GRPC_ERRORS_TOTAL: &str = "zaino.grpc.errors_total";
}
