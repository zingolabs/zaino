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

    // Throughput counters. `rate()` over these gives sync speed in
    // transactions/sec and shielded-actions/sec, which tracks indexing work more
    // faithfully than blocks/sec since per-block content varies wildly by height.
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
}
