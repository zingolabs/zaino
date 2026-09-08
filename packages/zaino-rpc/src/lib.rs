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
        // Retries are an `outcome`, not their own counter: with no denominator they read
        // the same under saturation and under growing load. `method` is not enumerable
        // (each caller names its own), so `zainod` exempts this from seeding
        counter RPC_OUTBOUND_REQUESTS_TOTAL = "zaino.rpc.outbound.requests_total" => "Outbound JSON-RPC attempts by method and outcome";
        // Separates "validator is slow" from "we ask too much"; the ingest histograms
        // cannot, since under `direct` their source read reaches no validator
        histogram RPC_OUTBOUND_DURATION_SECONDS = "zaino.rpc.outbound.duration_seconds" => "Seconds for one outbound JSON-RPC attempt that received a response, by method";
    }

    pub const RPC_METHOD: &str = "method";
    pub const RPC_OUTCOME: &str = "outcome";
}
