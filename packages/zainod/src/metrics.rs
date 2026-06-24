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
