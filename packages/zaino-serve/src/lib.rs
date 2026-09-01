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
    /// Serving latency per method; `_count` = request volume, so no extra counter.
    ///
    /// - Streaming methods: times *setup only* (the handler returns at stream
    ///   construction), hence the three stream metrics below — without them
    ///   `GetBlockRange` reports ~zero seconds
    pub const GRPC_REQUEST_DURATION_SECONDS: &str = "zaino.grpc.request_duration_seconds";
    pub const GRPC_ERRORS_TOTAL: &str = "zaino.grpc.errors_total";

    /// Server-stream lifetime: setup → last item or client hangup. What a range
    /// request costs.
    pub const GRPC_STREAM_SECONDS: &str = "zaino.grpc.stream_seconds";

    /// Items delivered over server streams, by method.
    ///
    /// - Into [`GRPC_STREAM_SECONDS`] = blocks/sec per method
    /// - Flushed at stream close, not per item (a lookup per block of every range
    ///   request is real cost) → `rate()` needs a window holding whole streams
    pub const GRPC_STREAM_ITEMS_TOTAL: &str = "zaino.grpc.stream_items_total";

    /// Streams currently open, by method.
    ///
    /// - A gauge: the failure is standing, not cumulative — an undrained stream
    ///   holds resources, and a duration histogram shows it only when it ends
    pub const GRPC_STREAMS_ACTIVE: &str = "zaino.grpc.streams_active";

    /// JSON-RPC serving latency by method; `_count` = request volume.
    ///
    /// - No counterpart to the gRPC pair existed, leaving block-explorer and
    ///   node-passthrough traffic — half of what zaino serves — invisible
    pub const JSONRPC_REQUEST_DURATION_SECONDS: &str = "zaino.jsonrpc.request_duration_seconds";
    pub const JSONRPC_ERRORS_TOTAL: &str = "zaino.jsonrpc.errors_total";

    /// Method label on every metric above.
    ///
    /// - Bounded on both surfaces, differently: gRPC from `stringify!` in the
    ///   handler macro (compile-time), JSON-RPC interned against the server's
    ///   method table (that surface can be asked for a method that does not exist)
    pub const SERVE_METHOD: &str = "method";

    /// Every histogram above, for `zainod` to check its bucket table against.
    /// Rationale: [`zaino_state::metric_names::HISTOGRAM_METRICS`].
    pub const HISTOGRAM_METRICS: [&str; 3] = [
        GRPC_REQUEST_DURATION_SECONDS,
        GRPC_STREAM_SECONDS,
        JSONRPC_REQUEST_DURATION_SECONDS,
    ];
}
