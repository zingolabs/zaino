//! Shared JSON-RPC 2.0 client.
//!
//! Handles the transport layer: HTTP requests, JSON-RPC envelope,
//! retry on work-queue exhaustion, authentication. Returns raw
//! `serde_json::Value` results — response parsing is the adapter's job.

mod client;
mod envelope;
mod error;
mod probe;
mod retry;

pub use client::{RpcClient, RpcClientConfig, HEAVY_METHOD_TIMEOUT, MAX_RESPONSE_BYTES};
pub use error::RpcError;
pub use probe::{auth_from_parts, probe_node, ProbeError};

/// Prometheus metric names emitted by this crate.
///
/// The single source of truth, shared with `zainod`'s `describe_*`
/// registrations, which carry the descriptions. Moved here from `zaino-fetch`
/// with the outbound RPC transport these name.
#[cfg(feature = "prometheus")]
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    pub const RPC_OUTBOUND_REQUESTS_TOTAL: &str = "zaino.rpc.outbound.requests_total";
    pub const RPC_OUTBOUND_REQUEST_DURATION_SECONDS: &str =
        "zaino.rpc.outbound.request_duration_seconds";
    pub const RPC_OUTBOUND_ERRORS_TOTAL: &str = "zaino.rpc.outbound.errors_total";
    pub const RPC_OUTBOUND_RETRIES_TOTAL: &str = "zaino.rpc.outbound.retries_total";
}
