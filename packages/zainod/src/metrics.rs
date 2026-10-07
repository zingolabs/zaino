//! Prometheus `/metrics` endpoint + the per-index metrics
//!
//! - Index metrics mirror each writer's committed view + the NFS's serving judgement
//! - NFS, sink, serve, chainview, validator-RPC + LSM metrics emitted by `zaino-nfs` /
//!   `zaino-sync` / `zaino-grpc` / `zaino-chainview` / `zaino-source` / `zaino-persistence`,
//!   registered here via their `describe_metrics` / `METRIC_BUCKETS`

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tracing::info;
use zaino_persistence::View as _;

use crate::error::IndexerError;

/// Dotted here, `_`-joined once scraped (`zaino.index.synced` → `zaino_index_synced`)
mod names {
    pub(super) const BUILD_INFO: &str = "zaino.build_info";
    pub(super) const INDEX_FINALIZED_HEIGHT: &str = "zaino.index.finalized_height";
    pub(super) const INDEX_SYNCED: &str = "zaino.index.synced";
}

/// Global recorder, served from [`crate::admin`] (own thread: answers however busy the workers)
pub(crate) fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    // Bind first (recorder before listener = undrained samples; bind failure fails startup)
    let listener = crate::admin::bind(endpoint)?;
    let builder = zaino_grpc::METRIC_BUCKETS
        .iter()
        .chain(zaino_source::METRIC_BUCKETS)
        .chain(zaino_chainview::METRIC_BUCKETS)
        .chain(zaino_persistence::lsm::METRIC_BUCKETS)
        .try_fold(PrometheusBuilder::new(), |builder, (metric, edges)| {
            builder.set_buckets_for_metric(Matcher::Full((*metric).to_owned()), edges)
        })
        .map_err(|e| IndexerError::MetricsError(format!("setting histogram buckets: {e}")))?;
    let handle = builder
        .install_recorder()
        .map_err(|e| IndexerError::MetricsError(format!("installing the recorder: {e}")))?;

    zaino_grpc::describe_metrics();
    zaino_chainview::describe_metrics();
    zaino_source::describe_metrics();
    zaino_nfs::describe_metrics();
    zaino_sync::describe_metrics();
    zaino_persistence::lsm::describe_metrics();
    describe_zainod();
    metrics::gauge!(names::BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    crate::admin::spawn(listener, handle)?;
    info!(%endpoint, "Listening");
    Ok(())
}

/// Process CPU, memory, fds sampled per scrape (sample age = answer age)
pub(crate) fn collect_process_metrics() {
    static COLLECTOR: std::sync::OnceLock<metrics_process::Collector> = std::sync::OnceLock::new();
    COLLECTOR
        .get_or_init(|| {
            let collector = metrics_process::Collector::default();
            collector.describe();
            collector
        })
        .collect();
}

fn describe_zainod() {
    use metrics::describe_gauge;

    describe_gauge!(names::BUILD_INFO, "Always 1; the zainod version rides the `version` label");
    describe_gauge!(
        names::INDEX_FINALIZED_HEIGHT,
        "Highest height the index has durably written, by index"
    );
    describe_gauge!(
        names::INDEX_SYNCED,
        "1 = served at the verified tip, 0 = the served tip trails it (still syncing), by index"
    );
}

/// Mirrors one index's committed tip + the NFS's serving judgement, labelled `index`
pub(crate) fn track_index(index: &'static str, watched: &crate::index_report::Watched) {
    let mut committed = watched.committed.clone();
    let mut synced = watched.synced.clone();

    tokio::spawn(async move {
        loop {
            let durable = committed.borrow_and_update().tip().map(|tip| tip.height);
            publish_finalized(index, durable);
            if committed.changed().await.is_err() {
                return;
            }
        }
    });

    tokio::spawn(async move {
        loop {
            publish_synced(index, *synced.borrow_and_update());
            if synced.changed().await.is_err() {
                return;
            }
        }
    });
}

/// Gauge = durable tip height, inclusive (unset while empty)
fn publish_finalized(index: &'static str, durable: Option<zaino_primitives::types::Height>) {
    if let Some(height) = durable {
        metrics::gauge!(names::INDEX_FINALIZED_HEIGHT, "index" => index)
            .set(f64::from(u32::from(height)));
    }
}

fn publish_synced(index: &'static str, serving: bool) {
    let value = if serving { 1.0 } else { 0.0 };
    metrics::gauge!(names::INDEX_SYNCED, "index" => index).set(value);
}
