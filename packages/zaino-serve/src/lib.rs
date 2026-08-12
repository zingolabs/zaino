//! Holds gRPC and JSON RPC servers capable of servicing clients over TCP.
//!
//! - server::ingestor has been built so that other ingestors may be added that use different transport protocols (Nym, TOR).
//!
//! Also holds rust implementations of the LightWallet gRPC Service (CompactTxStreamerServer).

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod rpc;
pub mod server;

/// Prometheus metric names emitted by this crate
#[cfg(feature = "prometheus")]
pub mod metric_names {
    /// Serving latency per method. Its `_count` is the request volume, so no
    pub const GRPC_REQUEST_DURATION_SECONDS: &str = "zaino.grpc.request_duration_seconds";
    pub const GRPC_ERRORS_TOTAL: &str = "zaino.grpc.errors_total";
}
