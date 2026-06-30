//! Prometheus metrics endpoint for Zaino.
//!
//! Installs a global metrics recorder and spawns an HTTP listener
//! that serves the `/metrics` scrape endpoint.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

// Metric names are owned by the crates that emit them, so the `describe_*`
// registrations below share one source of truth with the emit sites and can
// never drift.
use zaino_fetch::metric_names::*;
use zaino_serve::metric_names::*;
use zaino_state::metric_names::*;

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
        SYNC_FINALIZED_HEIGHT,
        "Current finalized block height being synced"
    );
    metrics::describe_gauge!(
        SYNC_TARGET_HEIGHT,
        "Target finalized block height for current sync iteration"
    );
    metrics::describe_gauge!(
        CHAIN_TIP_HEIGHT,
        "Latest chain tip height reported by the validator"
    );

    metrics::describe_counter!(
        SYNC_TRANSACTIONS_TOTAL,
        "Total transactions indexed during sync"
    );
    metrics::describe_counter!(
        SYNC_SAPLING_OUTPUTS_TOTAL,
        "Total Sapling outputs indexed during sync"
    );
    metrics::describe_counter!(
        SYNC_ORCHARD_ACTIONS_TOTAL,
        "Total Orchard actions indexed during sync"
    );

    metrics::describe_histogram!(
        SYNC_BLOCK_BUILD_SECONDS,
        "Seconds to fetch and build one indexed block (fetch + treestate + parse)"
    );
    metrics::describe_histogram!(
        SYNC_BLOCK_WRITE_SECONDS,
        "Seconds to durably write one batch of blocks to the database"
    );

    metrics::describe_gauge!(
        BUILD_INFO,
        "Static build metadata; always 1. Version exposed as a label."
    );

    // Sync lifecycle
    metrics::describe_gauge!(
        SYNC_HAS_REACHED_TIP,
        "Whether the indexer has ever reached the chain tip (0 or 1, never resets)"
    );
    metrics::describe_gauge!(
        SYNC_REACHED_TIP_AT,
        "Unix timestamp of the first time the indexer reached the chain tip"
    );
    metrics::describe_gauge!(
        SYNC_LAG_BLOCKS,
        "Number of blocks between chain tip and finalized height"
    );
    metrics::describe_counter!(
        SYNC_ITERATIONS_TOTAL,
        "Total sync loop iterations completed"
    );
    metrics::describe_histogram!(
        SYNC_ITERATION_DURATION_SECONDS,
        "Wall-clock duration of each sync loop iteration"
    );
    metrics::describe_counter!(
        SYNC_ERRORS_TOTAL,
        "Total sync loop errors by severity (recoverable or critical)"
    );
    metrics::describe_counter!(
        SYNC_REORG_TOTAL,
        "Total chain reorganization events detected in the non-finalized state"
    );
    metrics::describe_histogram!(
        SYNC_REORG_DEPTH,
        "Depth of chain reorganizations in blocks (0 for same-height reorgs)"
    );

    // DB
    metrics::describe_gauge!(
        DB_TIP_HEIGHT,
        "Height of the last block committed to the finalized database"
    );
    metrics::describe_gauge!(
        SYNC_LAST_BLOCK_WRITTEN_AT,
        "Unix timestamp of the last block written to the finalized database"
    );

    // Inbound gRPC
    metrics::describe_counter!(GRPC_REQUESTS_TOTAL, "Total inbound gRPC requests by method");
    metrics::describe_histogram!(
        GRPC_REQUEST_DURATION_SECONDS,
        "Duration of inbound gRPC requests by method"
    );
    metrics::describe_counter!(
        GRPC_ERRORS_TOTAL,
        "Total inbound gRPC errors by method and status code"
    );

    // Outbound JSON-RPC
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

    // Mempool
    metrics::describe_gauge!(
        MEMPOOL_TRANSACTIONS,
        "Current number of transactions in the mempool"
    );
    metrics::describe_counter!(
        MEMPOOL_TIP_CHANGES_TOTAL,
        "Total mempool resets due to chain tip changes"
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
