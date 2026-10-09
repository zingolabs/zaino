//! Prometheus `/metrics` endpoint
//!
//! - State gauges (tips, handed, per-index durable + synced, `zaino_chainview_*`) =
//!   `zaino-snapshot`'s, set from one snapshot load per scrape ([`crate::status::emit_gauges`])
//! - Events emitted where they happen (`zaino-nfs` / `zaino-sync` / `zaino-grpc` /
//!   `zaino-chainview` / `zaino-source` / `zaino-persistence`), registered here via their
//!   `describe_metrics` / `METRIC_BUCKETS`

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::info;

use crate::error::IndexerError;

/// Dotted here, `_`-joined once scraped (`zaino.build_info` → `zaino_build_info`)
const BUILD_INFO: &str = "zaino.build_info";

/// Global recorder, served from [`crate::admin`] (own thread: answers however busy the workers)
pub(crate) fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    // Bind first (recorder before listener = undrained samples; bind failure fails startup)
    let listener = crate::admin::bind(endpoint)?;
    let builder = zaino_grpc::METRIC_BUCKETS
        .iter()
        .chain(zaino_source::METRIC_BUCKETS)
        .chain(zaino_chainview::METRIC_BUCKETS)
        .chain(zaino_persistence::METRIC_BUCKETS)
        .chain(zaino_persistence::lsm::METRIC_BUCKETS)
        .try_fold(PrometheusBuilder::new(), |builder, (metric, edges)| {
            builder.set_buckets_for_metric(Matcher::Full((*metric).to_owned()), edges)
        })
        .map_err(|e| IndexerError::MetricsError(format!("setting histogram buckets: {e}")))?;
    let handle = builder
        .install_recorder()
        .map_err(|e| IndexerError::MetricsError(format!("installing the recorder: {e}")))?;

    describe_all();
    crate::admin::spawn(listener, handle)?;
    info!(%endpoint, "Listening");
    Ok(())
}

/// One scrape: snapshot gauges + process sample set, then rendered (blocking: walks every handle)
pub(crate) fn scrape(handle: &PrometheusHandle) -> String {
    crate::status::emit_gauges();
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
    zaino_grpc::describe_metrics();
    zaino_chainview::describe_metrics();
    zaino_source::describe_metrics();
    zaino_nfs::describe_metrics();
    zaino_snapshot::describe_metrics();
    zaino_sync::describe_metrics();
    zaino_persistence::describe_metrics();
    crate::disk_monitor::describe_metrics();
    metrics::describe_gauge!(BUILD_INFO, "Always 1; the zainod version rides the `version` label");
    metrics::gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
}
