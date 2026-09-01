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
    /// Every outbound call, by `method` and [`RPC_OUTCOME`].
    ///
    /// - Retries are an `outcome` here, not a standalone counter: with no
    ///   denominator they read the same under saturation and under growing load
    /// - Only the two ingest calls had per-block volume; `getbestblockheight`,
    ///   mempool polls & passthrough RPCs were unmeasured, and are most of `rpc`
    pub const RPC_OUTBOUND_REQUESTS_TOTAL: &str = "zaino.rpc.outbound.requests_total";

    /// Round-trip latency of outbound calls that returned a result, by `method`.
    ///
    /// - Separates "validator is slow" from "we ask too much"; the ingest
    ///   histograms cannot — under `direct` their source read has no validator
    pub const RPC_OUTBOUND_DURATION_SECONDS: &str = "zaino.rpc.outbound.duration_seconds";

    /// JSON-RPC method called; bounded by the validator's API surface.
    pub const RPC_METHOD: &str = "method";

    /// How an outbound call ended; values in [`RPC_OUTCOMES`].
    pub const RPC_OUTCOME: &str = "outcome";

    /// Every [`RPC_OUTCOME`] value.
    ///
    /// - `transport_error` is the addition: retries counted only retryable
    ///   JSON-RPC *codes*, so HTTP failure / refusal / timeout moved no metric
    pub const RPC_OUTCOMES: [&str; 4] = ["ok", "rpc_error", "retried", "transport_error"];

    /// Every histogram above, for `zainod` to check its bucket table against.
    /// Rationale: [`zaino_state::metric_names::HISTOGRAM_METRICS`].
    pub const HISTOGRAM_METRICS: [&str; 1] = [RPC_OUTBOUND_DURATION_SECONDS];
}
