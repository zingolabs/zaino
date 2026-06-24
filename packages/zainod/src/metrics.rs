//! Prometheus metrics endpoint for Zaino.
//!
//! Installs a global metrics recorder and spawns an HTTP listener
//! that serves the `/metrics` scrape endpoint.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

use crate::error::IndexerError;

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
        "zaino.sync.finalized_height",
        "Current finalized block height being synced"
    );
    metrics::describe_gauge!(
        "zaino.sync.target_height",
        "Target finalized block height for current sync iteration"
    );
    metrics::describe_gauge!(
        "zaino.chain.tip_height",
        "Latest chain tip height reported by the validator"
    );

    metrics::describe_counter!(
        "zaino.sync.transactions_total",
        "Total transactions indexed during sync"
    );
    metrics::describe_counter!(
        "zaino.sync.sapling_outputs_total",
        "Total Sapling outputs indexed during sync"
    );
    metrics::describe_counter!(
        "zaino.sync.orchard_actions_total",
        "Total Orchard actions indexed during sync"
    );

    metrics::describe_histogram!(
        "zaino.sync.block_build_seconds",
        "Seconds to fetch and build one indexed block (fetch + treestate + parse)"
    );
    metrics::describe_histogram!(
        "zaino.sync.block_write_seconds",
        "Seconds to durably write one batch of blocks to the database"
    );

    metrics::describe_gauge!(
        "zainod.build_info",
        "Static build metadata; always 1. Version exposed as a label."
    );

    // Sync lifecycle
    metrics::describe_gauge!(
        "zaino.sync.has_reached_tip",
        "Whether the indexer has ever reached the chain tip (0 or 1, never resets)"
    );
    metrics::describe_gauge!(
        "zaino.sync.reached_tip_at",
        "Unix timestamp of the first time the indexer reached the chain tip"
    );
    metrics::describe_gauge!(
        "zaino.sync.lag_blocks",
        "Number of blocks between chain tip and finalized height"
    );
    metrics::describe_counter!(
        "zaino.sync.iterations_total",
        "Total sync loop iterations completed"
    );
    metrics::describe_histogram!(
        "zaino.sync.iteration_duration_seconds",
        "Wall-clock duration of each sync loop iteration"
    );
    metrics::describe_counter!(
        "zaino.sync.errors_total",
        "Total sync loop errors by severity (recoverable or critical)"
    );
    metrics::describe_counter!(
        "zaino.sync.reorg_total",
        "Total chain reorganization events detected in the non-finalized state"
    );
    metrics::describe_histogram!(
        "zaino.sync.reorg_depth",
        "Depth of chain reorganizations in blocks (0 for same-height reorgs)"
    );

    // DB
    metrics::describe_gauge!(
        "zaino.db.tip_height",
        "Height of the last block committed to the finalized database"
    );
    metrics::describe_gauge!(
        "zaino.sync.last_block_written_at",
        "Unix timestamp of the last block written to the finalized database"
    );

    // Inbound gRPC
    metrics::describe_counter!(
        "zaino.grpc.requests_total",
        "Total inbound gRPC requests by method"
    );
    metrics::describe_histogram!(
        "zaino.grpc.request_duration_seconds",
        "Duration of inbound gRPC requests by method"
    );
    metrics::describe_counter!(
        "zaino.grpc.errors_total",
        "Total inbound gRPC errors by method and status code"
    );

    // Outbound JSON-RPC
    metrics::describe_counter!(
        "zaino.rpc.outbound.requests_total",
        "Total outbound JSON-RPC requests by method"
    );
    metrics::describe_histogram!(
        "zaino.rpc.outbound.request_duration_seconds",
        "Duration of outbound JSON-RPC requests by method"
    );
    metrics::describe_counter!(
        "zaino.rpc.outbound.errors_total",
        "Total outbound JSON-RPC errors by method"
    );
    metrics::describe_counter!(
        "zaino.rpc.outbound.retries_total",
        "Total outbound JSON-RPC retries due to work queue depth exceeded"
    );

    // Mempool
    metrics::describe_gauge!(
        "zaino.mempool.transactions",
        "Current number of transactions in the mempool"
    );
    metrics::describe_counter!(
        "zaino.mempool.tip_changes_total",
        "Total mempool resets due to chain tip changes"
    );
}

/// Emit a constant gauge `zainod_build_info{version="x.y.z"} 1` so the
/// deployed binary version is queryable in PromQL / Grafana, matching the
/// pattern Zebra uses with `zebrad_build_info`.
fn set_build_info() {
    metrics::gauge!(
        "zainod.build_info",
        "version" => env!("CARGO_PKG_VERSION"),
    )
    .set(1.0);
}
