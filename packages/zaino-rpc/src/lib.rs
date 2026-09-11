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
    zaino_status::metric_names! {
        // Errors only, mirroring the inbound gRPC & JSON-RPC surfaces: volume is the
        // duration histogram's `_count`, so a success counter would only restate it.
        // `method` is not enumerable (each caller names its own) → exempt from seeding
        counter RPC_OUTBOUND_ERRORS_TOTAL = "zaino.rpc.outbound.errors_total" => "Failed outbound JSON-RPC attempts by method and outcome: unreachable, refused, or retried";
        // Separates "validator is slow" from "we ask too much"; the ingest histograms
        // cannot, since under `direct` their source read reaches no validator
        histogram RPC_OUTBOUND_DURATION_SECONDS = "zaino.rpc.outbound.duration_seconds" => "Seconds for one outbound JSON-RPC attempt that received a response, by method";
    }

    pub const RPC_METHOD: &str = "method";

    /// `transport_error` unreachable / `rpc_error` refused / `retried` saturated —
    /// three different operator actions, none derivable from the histogram
    pub const RPC_OUTCOME: &str = "outcome";
}
