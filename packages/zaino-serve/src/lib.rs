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
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    zaino_status::metric_names! {
        // `_count` carries the request volume, so neither surface has a request counter
        histogram GRPC_REQUEST_DURATION_SECONDS = "zaino.grpc.request_duration_seconds" => "Seconds serving one inbound gRPC request, by method; streaming methods time setup only";
        histogram JSONRPC_REQUEST_DURATION_SECONDS = "zaino.jsonrpc.request_duration_seconds" => "Seconds serving one inbound JSON-RPC request, by method";
        // Both carry `method`, so `zainod` exempts them from seeding
        counter GRPC_ERRORS_TOTAL = "zaino.grpc.errors_total" => "Inbound gRPC errors by method and canonical status name";
        counter JSONRPC_ERRORS_TOTAL = "zaino.jsonrpc.errors_total" => "Inbound JSON-RPC errors by method and zcashd-compatible error code";
    }

    /// Cardinality bounded per surface: gRPC by `stringify!` in the handler macro,
    /// JSON-RPC interned against the method table (callers name methods)
    pub const SERVE_METHOD: &str = "method";

    /// gRPC status name (`NotFound`) or JSON-RPC numeric code; neither caller-supplied
    pub const SERVE_CODE: &str = "code";
}
