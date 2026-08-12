//! Prometheus metrics endpoint for Zaino.
//!
//! Installs a global metrics recorder and spawns an HTTP listener
//! that serves the `/metrics` scrape endpoint.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
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

/// Explicit bucket bounds for the latency histograms.
const LATENCY_BUCKETS: [(&str, &[f64]); 2] = [
    (
        "_block_fetch_seconds",
        &[
            0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ],
    ),
    (
        "_seconds",
        &[
            0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
        ],
    ),
];

/// Install the Prometheus metrics recorder and spawn the HTTP listener.
///
/// This must be called **once** before any `metrics::gauge!()` / `metrics::counter!()`
/// calls, otherwise those calls silently no-op.
pub fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    let mut builder = PrometheusBuilder::new().with_http_listener(endpoint);
    for (suffix, buckets) in LATENCY_BUCKETS {
        builder = builder
            .set_buckets_for_metric(Matcher::Suffix(suffix.to_string()), buckets)
            .map_err(|e| {
                IndexerError::MetricsError(format!(
                    "Failed to set histogram buckets for `*{suffix}`: {e}"
                ))
            })?;
    }
    builder.install().map_err(|e| {
        IndexerError::MetricsError(format!("Failed to install metrics recorder: {e}"))
    })?;

    describe_metrics();
    initialise_counters();
    set_build_info();

    info!(%endpoint, "Prometheus metrics endpoint started");
    Ok(())
}

/// Register human-readable descriptions for all Zaino metrics.
///
/// These appear as `# HELP` lines in the scrape output.
fn describe_metrics() {
    // Progress. Lag is `chain_tip - finalized`, derived by the consumer; no
    // pre-derived lag gauge is exported.
    metrics::describe_gauge!(
        CHAIN_TIP_HEIGHT,
        "Latest chain tip height reported by the validator"
    );
    metrics::describe_gauge!(
        SYNC_FINALIZED_HEIGHT,
        "Height the finalized index is committed to: written and fsynced, so a crash cannot lose it"
    );
    metrics::describe_gauge!(
        SYNC_FETCHED_HEIGHT,
        "Height the sync loop has built to in memory; advances per block, ahead of the next commit"
    );
    metrics::describe_gauge!(
        SYNC_TARGET_HEIGHT,
        "Height the write path is working towards: chain tip minus the non-finalised reorg buffer. \
         Sync is complete against this, not against the raw chain tip"
    );

    // Health. The depth histogram's _count is the reorg total.
    metrics::describe_histogram!(
        SYNC_REORG_DEPTH,
        "Depth of chain reorganizations in blocks (0 for same-height reorgs); \
         its _count is the total reorg event count"
    );

    // Cost attribution.
    metrics::describe_histogram!(
        SYNC_BLOCK_BUILD_SECONDS,
        "Seconds to fetch and build one indexed block (fetch + treestate + parse)"
    );
    metrics::describe_histogram!(
        SYNC_BLOCK_FETCH_SECONDS,
        "Seconds awaiting the validator for one block (get_block + commitment tree roots); \
         subtract from block_build_seconds for zaino's own parse cost"
    );
    metrics::describe_histogram!(
        SYNC_BATCH_WRITE_SECONDS,
        "Seconds to durably write one batch of blocks to the database, including the fsync"
    );

    // Throughput, per value pool. A rate over any of these is operations per
    // second; see `BlockWork` in zaino-state's write path for why they are not
    // tied to the batch commit.
    metrics::describe_counter!(
        SYNC_TRANSACTIONS_TOTAL,
        "Total transactions ingested during sync"
    );
    metrics::describe_counter!(
        SYNC_TRANSPARENT_OPS_TOTAL,
        "Total transparent inputs and outputs ingested during sync"
    );
    metrics::describe_counter!(
        SYNC_SAPLING_OPS_TOTAL,
        "Total Sapling spends and outputs ingested during sync"
    );
    metrics::describe_counter!(
        SYNC_ORCHARD_ACTIONS_TOTAL,
        "Total Orchard actions ingested during sync"
    );
    metrics::describe_counter!(
        SYNC_IRONWOOD_ACTIONS_TOTAL,
        "Total Ironwood actions ingested during sync"
    );

    // Storage. Against host RAM, used_bytes is where write throughput changes
    // character; against map_size it is how close the database is to full.
    metrics::describe_gauge!(DB_USED_BYTES, "Bytes in use by the LMDB environment");
    metrics::describe_gauge!(DB_MAP_SIZE_BYTES, "Bytes the LMDB map is sized to");

    // Inbound gRPC. The duration histogram's _count is the request volume.
    metrics::describe_histogram!(
        GRPC_REQUEST_DURATION_SECONDS,
        "Duration of inbound gRPC requests by method; its _count is the request volume. \
         Streaming methods time stream setup, not delivery"
    );
    metrics::describe_counter!(
        GRPC_ERRORS_TOTAL,
        "Total inbound gRPC errors by method and status code"
    );

    // Outbound JSON-RPC.
    metrics::describe_counter!(
        RPC_OUTBOUND_RETRIES_TOTAL,
        "Total outbound JSON-RPC retries due to work queue depth exceeded; \
         a rising rate means the validator is saturated and more concurrency will not help"
    );

    metrics::describe_gauge!(
        BUILD_INFO,
        "Static build metadata; always 1. Version exposed as a label."
    );
}

fn initialise_counters() {
    for name in [
        SYNC_TRANSACTIONS_TOTAL,
        SYNC_TRANSPARENT_OPS_TOTAL,
        SYNC_SAPLING_OPS_TOTAL,
        SYNC_ORCHARD_ACTIONS_TOTAL,
        SYNC_IRONWOOD_ACTIONS_TOTAL,
    ] {
        metrics::counter!(name).increment(0);
    }
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

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// The work counters are scrapeable before anything has been indexed.
    ///
    /// This is the property a consumer depends on to tell "nothing indexed yet"
    /// from "this build does not report it". Asserting on the *rendered* scrape
    /// rather than on the calls is the point: `describe_counter!` alone also
    /// "registers" a metric, and would pass any test that only checked the
    /// descriptions.
    #[test]
    fn counters_are_scrapeable_before_their_first_increment() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            initialise_counters();
        });

        let scrape = handle.render();
        for series in [
            "zaino_sync_transactions_total",
            "zaino_sync_transparent_ops_total",
            "zaino_sync_sapling_ops_total",
            "zaino_sync_orchard_actions_total",
            "zaino_sync_ironwood_actions_total",
        ] {
            assert!(
                scrape.contains(&format!("{series} 0")),
                "`{series}` is absent from a fresh scrape, so a consumer cannot \
                 tell zero from unsupported. Scrape was:\n{scrape}"
            );
        }
    }

    /// Gauges are deliberately *not* pre-created: zero is a false reading for a
    /// height, so absent is the honest answer until something measures one.
    #[test]
    fn height_gauges_are_absent_until_measured() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            initialise_counters();
        });

        let scrape = handle.render();
        assert!(
            !scrape.contains("zaino_sync_finalized_height "),
            "a finalized-height gauge was published before any block was \
             indexed, which reads as a tip at genesis. Scrape was:\n{scrape}"
        );
    }

    /// Latency metrics must scrape as **histograms**, not summaries.
    ///
    /// Without explicit buckets `metrics-exporter-prometheus` renders every
    /// histogram as a summary: quantiles over a short rolling window, which
    /// cannot be aggregated across instances or re-windowed in a query, and
    /// which costs a sketch update per observation on a path that runs once per
    /// block. The failure is silent — the series still appears, it just answers
    /// a different question — so it is asserted on the rendered output.
    #[test]
    fn latency_metrics_scrape_as_bucketed_histograms() {
        let mut builder = PrometheusBuilder::new();
        for (suffix, buckets) in LATENCY_BUCKETS {
            builder = builder
                .set_buckets_for_metric(Matcher::Suffix(suffix.to_string()), buckets)
                .expect("bucket bounds are non-empty and finite");
        }
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            metrics::histogram!(SYNC_BLOCK_FETCH_SECONDS).record(0.004);
            metrics::histogram!(SYNC_BLOCK_BUILD_SECONDS).record(0.05);
        });

        let scrape = handle.render();
        for series in [
            "zaino_sync_block_fetch_seconds",
            "zaino_sync_block_build_seconds",
        ] {
            assert!(
                scrape.contains(&format!("{series}_bucket")),
                "`{series}` rendered without buckets, so it is a summary and \
                 histogram_quantile() over it is unavailable. Scrape was:\n{scrape}"
            );
        }
        // The fetch ladder starts finer than the shared one: a sub-millisecond
        // validator round trip must not land in the same bucket as a 5 ms one.
        assert!(
            scrape.contains(r#"zaino_sync_block_fetch_seconds_bucket{le="0.0005"}"#),
            "the fetch-specific bucket ladder was not applied; the generic \
             `_seconds` ladder matched first. Scrape was:\n{scrape}"
        );
    }
}
