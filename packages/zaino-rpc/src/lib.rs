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
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    // Errors only: volume = the duration histogram's `_count` (as inbound gRPC & JSON-RPC)
    pub const RPC_OUTBOUND_ERRORS_TOTAL: &str = "zaino.rpc.outbound.errors_total";
    // Slow validator vs too many asks (ingest histograms can't tell: `direct` reads reach no validator)
    pub const RPC_OUTBOUND_DURATION_SECONDS: &str = "zaino.rpc.outbound.duration_seconds";

    pub const RPC_METHOD: &str = "method";

    /// `transport_error` unreachable / `rpc_error` refused / `retried` saturated
    pub const RPC_OUTCOME: &str = "outcome";

    #[rustfmt::skip]
    pub const COUNTERS: &[(&str, &str)] = &[
        (RPC_OUTBOUND_ERRORS_TOTAL, "Failed outbound JSON-RPC attempts by method and outcome: unreachable, refused, or retried"),
    ];

    #[rustfmt::skip]
    pub const HISTOGRAMS: &[(&str, &str)] = &[
        (RPC_OUTBOUND_DURATION_SECONDS, "Seconds for one outbound JSON-RPC attempt that received a response, by method"),
    ];
}
