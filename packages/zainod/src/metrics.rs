//! Prometheus `/metrics` endpoint + the per-index metrics
//!
//! - Index metrics mirror each follower's watches
//! - Producer, serve, validator-RPC + LSM metrics emitted by `zaino-sync` / `zaino-grpc` /
//!   `zaino-source` / `zaino-persistence`, registered here via their `describe_metrics` /
//!   `METRIC_BUCKETS`

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tracing::info;

use crate::error::IndexerError;

/// Dotted here, `_`-joined once scraped (`zaino.build_info` → `zaino_build_info`)
mod names {
    pub(super) const BUILD_INFO: &str = "zaino.build_info";
    pub(super) const INDEX_FINALIZED_HEIGHT: &str = "zaino.index.finalized_height";
    pub(super) const INDEX_SYNCED: &str = "zaino.index.synced";
}

/// Installs the global recorder + HTTP listener (before it, every emit site no-ops)
///
/// - listener on its own thread + current-thread runtime: a scrape answers however busy the
///   serving and sync workers are
pub(crate) fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    let builder = zaino_grpc::METRIC_BUCKETS
        .iter()
        .chain(zaino_source::METRIC_BUCKETS)
        .chain(zaino_persistence::lsm::METRIC_BUCKETS)
        .try_fold(
            PrometheusBuilder::new().with_http_listener(endpoint),
            |builder, (metric, edges)| {
                builder.set_buckets_for_metric(Matcher::Full((*metric).to_owned()), edges)
            },
        )
        .map_err(|e| IndexerError::MetricsError(format!("setting histogram buckets: {e}")))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| IndexerError::MetricsError(format!("building its runtime: {e}")))?;
    let (recorder, exporter) = {
        let _entered = runtime.enter();
        builder.build().map_err(|e| IndexerError::MetricsError(format!("building: {e}")))?
    };
    std::thread::Builder::new()
        .name("metrics".to_owned())
        .spawn(move || runtime.block_on(exporter))
        .map_err(|e| IndexerError::MetricsError(format!("spawning its thread: {e}")))?;
    metrics::set_global_recorder(recorder)
        .map_err(|e| IndexerError::MetricsError(format!("installing the recorder: {e}")))?;

    zaino_grpc::describe_metrics();
    zaino_source::describe_metrics();
    zaino_sync::describe_metrics();
    zaino_persistence::lsm::describe_metrics();
    describe_zainod();
    metrics::gauge!(names::BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    info!(%endpoint, "Listening");
    Ok(())
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
        "1 = the index serves, 0 = it refuses every request as syncing, by index"
    );
}

/// Mirrors one follower's durable extent + sync gate, labelled `S::NAME`
pub(crate) fn track_index<S: zaino_sync::IndexWriter>(follower: &zaino_sync::IndexFollower<S>) {
    let index = S::NAME;
    let mut finalized = follower.subscribe_finalized();
    let mut synced = follower.subscribe_synced();

    tokio::spawn(async move {
        loop {
            publish_finalized(index, *finalized.borrow_and_update());
            if finalized.changed().await.is_err() {
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

/// Gauge = highest durable height (unset while empty)
fn publish_finalized(index: &'static str, durable: zaino_primitives::types::Extent) {
    if let Some(height) = durable.last() {
        metrics::gauge!(names::INDEX_FINALIZED_HEIGHT, "index" => index)
            .set(f64::from(u32::from(height)));
    }
}

fn publish_synced(index: &'static str, serving: bool) {
    let value = if serving { 1.0 } else { 0.0 };
    metrics::gauge!(names::INDEX_SYNCED, "index" => index).set(value);
}
