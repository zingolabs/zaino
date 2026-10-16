//! Prometheus `/metrics` endpoint
//!
//! - State gauges (tips, handed, per-index durable + synced, `zaino_chainview_*`) =
//!   `zaino-grpc-snapshot`'s, set from one `GrpcSnapshot` load per scrape
//!   ([`DaemonStatus::emit_gauges`])
//! - Events emitted where they happen, registered here from each crate's `METRICS` ([`TABLES`])

use std::{net::SocketAddr, sync::Arc};

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::info;
use zaino_primitives::metric::Metric;

use crate::error::IndexerError;
use crate::status::DaemonStatus;

/// Dotted here, `_`-joined once scraped (`zaino.build_info` → `zaino_build_info`)
const BUILD_INFO: &str = "zaino.build_info";

/// Every emitting crate's table (a new crate = one row)
const TABLES: &[&[Metric]] = &[
    zaino_grpc::METRICS,
    zaino_grpc_snapshot::METRICS,
    zaino_chainview::METRICS,
    zaino_traffic::METRICS,
    zaino_source::METRICS,
    zaino_nfs::METRICS,
    zaino_sync::METRICS,
    zaino_persistence::METRICS,
    crate::disk_monitor::METRICS,
];

fn every_metric() -> impl Iterator<Item = &'static Metric> {
    TABLES.iter().copied().flatten()
}

/// Global recorder, served from [`crate::admin`] (own thread: answers however busy the workers)
pub(crate) fn init(endpoint: SocketAddr, status: Arc<DaemonStatus>) -> Result<(), IndexerError> {
    // - Bind first (recorder before listener = undrained samples)
    // - Bind failure fails startup
    let listener = crate::admin::bind(endpoint)?;
    let builder = every_metric()
        .filter_map(|metric| Some((metric.name(), metric.buckets()?)))
        .try_fold(PrometheusBuilder::new(), |builder, (name, edges)| {
            builder.set_buckets_for_metric(Matcher::Full(name.to_owned()), edges)
        })
        .map_err(|e| IndexerError::Metrics(format!("setting histogram buckets: {e}")))?;
    let handle = builder
        .install_recorder()
        .map_err(|e| IndexerError::Metrics(format!("installing the recorder: {e}")))?;

    describe_all();
    crate::admin::spawn(listener, handle, status)?;
    info!(%endpoint, "Listening");
    Ok(())
}

/// One scrape: `GrpcSnapshot` gauges + process sample set, rendered (blocking: walks every handle)
pub(crate) fn scrape(handle: &PrometheusHandle, status: &DaemonStatus) -> String {
    status.emit_gauges();
    collect_process_metrics();
    handle.render()
}

/// Process CPU, memory, fds sampled per scrape (sample age = answer age)
fn collect_process_metrics() {
    static COLLECTOR: std::sync::OnceLock<metrics_process::Collector> = std::sync::OnceLock::new();
    COLLECTOR
        .get_or_init(|| {
            let collector = metrics_process::Collector::default();
            collector.describe();
            collector
        })
        .collect();
}

/// Every crate's `# HELP` + the build-info gauge (into the current recorder)
pub(crate) fn describe_all() {
    every_metric().for_each(Metric::describe);
    metrics::describe_gauge!(BUILD_INFO, "Always 1; the zainod version rides the `version` label");
    metrics::gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
}
