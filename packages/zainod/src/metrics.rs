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
/// This must be called **once** before any `metrics::gauge!()` / `metrics::histogram!()`
/// calls, otherwise those calls silently no-op.
pub fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    PrometheusBuilder::new()
        .with_http_listener(endpoint)
        .install()
        .map_err(|e| {
            IndexerError::MetricsError(format!("Failed to install metrics recorder: {e}"))
        })?;

    describe_metrics();

    info!(%endpoint, "Prometheus metrics endpoint started");
    Ok(())
}

/// Register human-readable descriptions for all Zaino metrics.
///
/// These appear as `# HELP` lines in the scrape output.
fn describe_metrics() {
    // Sync progress gauges.
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

    // Per-block sync-phase timings. Comparing these across height isolates which
    // phase's latency grows (e.g. block fetch vs. commitment-tree fetch) at a
    // network-upgrade boundary.
    metrics::describe_histogram!(
        "zaino.sync.block_fetch_seconds",
        "Seconds to fetch one block from the validator (RPC #1)"
    );
    metrics::describe_histogram!(
        "zaino.sync.treestate_fetch_seconds",
        "Seconds to fetch one block's Sapling/Orchard commitment-tree roots (RPC #2)"
    );
    metrics::describe_histogram!(
        "zaino.sync.block_build_seconds",
        "Seconds to build the indexed block in-process (CPU-bound phase)"
    );
    metrics::describe_histogram!(
        "zaino.sync.block_write_seconds",
        "Seconds to durably write one batch of blocks to the database"
    );
}
