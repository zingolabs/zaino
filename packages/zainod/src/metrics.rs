//! Prometheus metrics endpoint for Zaino.
//!
//! Installs a global metrics recorder and spawns an HTTP listener
//! that serves the `/metrics` scrape endpoint.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

// Metric names are owned by the crates that emit them, so the `describe_*`
// registrations below share one source of truth with the emit sites and can
// never drift. On the runtime stack the only emitter wired so far is the
// outbound JSON-RPC client (`zaino-rpc`); the sync/DB/gRPC/mempool metric sets
// that the legacy serving stack described are re-added here as the new stack's
// components start emitting them.
use zaino_rpc::metric_names::*;

use crate::error::IndexerError;

/// Static build-metadata gauge name (`zainod.build_info`); see [`set_build_info`].
const BUILD_INFO: &str = "zainod.build_info";

/// Install the Prometheus metrics recorder and spawn the HTTP listener.
///
/// This must be called **once** before any `metrics::gauge!()` / `metrics::counter!()`
/// calls, otherwise those calls silently no-op.
pub fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    PrometheusBuilder::new()
        .with_http_listener(endpoint)
        .install()
        .map_err(|e| {
            IndexerError::MetricsError(format!("Failed to install metrics recorder: {e}"))
        })?;

    describe_metrics();
    set_build_info();

    info!(%endpoint, "Prometheus metrics endpoint started");
    Ok(())
}

/// Register human-readable descriptions for all Zaino metrics.
///
/// These appear as `# HELP` lines in the scrape output.
fn describe_metrics() {
    metrics::describe_gauge!(
        BUILD_INFO,
        "Static build metadata; always 1. Version exposed as a label."
    );

    // Outbound JSON-RPC (the validator connection) — the only metrics the
    // runtime stack emits so far. Sync / DB / inbound-gRPC / mempool sets are
    // re-added as the runtime components gain their own metric names.
    metrics::describe_counter!(
        RPC_OUTBOUND_REQUESTS_TOTAL,
        "Total outbound JSON-RPC requests by method"
    );
    metrics::describe_histogram!(
        RPC_OUTBOUND_REQUEST_DURATION_SECONDS,
        "Duration of outbound JSON-RPC requests by method"
    );
    metrics::describe_counter!(
        RPC_OUTBOUND_ERRORS_TOTAL,
        "Total outbound JSON-RPC errors by method"
    );
    metrics::describe_counter!(
        RPC_OUTBOUND_RETRIES_TOTAL,
        "Total outbound JSON-RPC retries due to work queue depth exceeded"
    );
}

/// Emit a constant gauge `zainod_build_info{version="x.y.z"} 1` so the
/// deployed binary version is queryable in PromQL / Grafana, matching the
/// pattern Zebra uses with `zebrad_build_info`.
fn set_build_info() {
    metrics::gauge!(
        BUILD_INFO,
        "version" => env!("CARGO_PKG_VERSION"),
    )
    .set(1.0);
}
