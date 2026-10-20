//! Prometheus recorder + scrape rendering
//!
//! - State gauges (tips, handed, per-index durable + synced, `zaino_chainview_*`) =
//!   `zaino-grpc-snapshot`'s, set from one `GrpcSnapshot` load per scrape
//!   ([`DaemonStatus::emit_gauges`])
//! - Events emitted where they happen (`zaino-nfs` / `zaino-sync` / `zaino-grpc` /
//!   `zaino-chainview` / `zaino-source` / `zaino-persistence`), described here via their
//!   `describe_metrics`
//! - Every histogram native (exponential buckets, range rescales itself) = protobuf scrapes only
//! - zebra-network's series carry no peer identity ([`WithoutPeerIdentity`])

use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Label, Metadata, Recorder, SharedString, Unit,
};
use metrics_exporter_prometheus::{
    Matcher, NativeHistogramConfig, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};

use crate::error::IndexerError;
use crate::status::DaemonStatus;

/// Dotted here, `_`-joined once scraped (`zaino.build_info` → `zaino_build_info`)
const BUILD_INFO: &str = "zaino.build_info";

/// ~9% wide buckets (schema 3 = Prometheus' default native resolution)
const BUCKET_FACTOR: f64 = 1.1;

/// Past it, bucket width doubles (schema − 1) instead of dropping samples
const MAX_BUCKETS: u32 = 160;

/// `/metrics` exposition, negotiated per scrape from `Accept`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// No native histograms (the text format cannot carry them)
    Text,
    Protobuf,
}

impl Format {
    pub(crate) fn content_type(self) -> &'static str {
        match self {
            // Load-bearing: Prometheus >= 3.0 drops a scrape with a blank `Content-Type`
            Format::Text => "text/plain; version=0.0.4; charset=utf-8",
            Format::Protobuf => {
                "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; \
                 encoding=delimited"
            }
        }
    }
}

/// Global recorder + every `# HELP` (served by [`crate::admin::start`], which binds first)
pub(crate) fn install_recorder() -> Result<PrometheusHandle, IndexerError> {
    let (recorder, handle) = recorder()?;
    metrics::set_global_recorder(recorder)
        .map_err(|e| IndexerError::Metrics(format!("installing the recorder: {e}")))?;

    describe_all();
    Ok(handle)
}

/// zainod's recorder (tests' too) + its scrape handle
///
/// - every histogram native (empty prefix = every metric name, zebra-network's included)
pub(crate) fn recorder(
) -> Result<(WithoutPeerIdentity<PrometheusRecorder>, PrometheusHandle), IndexerError> {
    let native = NativeHistogramConfig::new(BUCKET_FACTOR, MAX_BUCKETS, 0.0)
        .map_err(|e| IndexerError::Metrics(format!("native histogram config: {e}")))?;
    let recorder = PrometheusBuilder::new()
        .set_native_histogram_for_metric(Matcher::Prefix(String::new()), native)
        .build_recorder();
    let handle = recorder.handle();
    Ok((WithoutPeerIdentity(recorder), handle))
}

/// zebra-network's metrics minus who the peer is (its address, or free text it chose: unbounded
/// series, third-party IPs shipped to wherever the scrape goes)
///
/// - counter / histogram: [`PEER_LABELS`] dropped (one total per remaining label set)
/// - gauge with one: dropped whole (one peer's reading: its version, its connection's state;
///   never a sum)
pub(crate) struct WithoutPeerIdentity<R>(R);

/// zebra-network's metric name prefixes (zaino's own carry no peer identity)
const PEER_METRICS: [&str; 2] = ["zcash.net.", "zebra.net."];

const PEER_LABELS: [&str; 6] =
    ["addr", "remote_ip", "user_agent", "remote_version", "remote_services", "error"];

/// `Some` = `key` minus its peer labels (`None` = had none)
fn without_peer(key: &Key) -> Option<Key> {
    if !PEER_METRICS.iter().any(|prefix| key.name().starts_with(prefix)) {
        return None;
    }
    let peer = |label: &Label| PEER_LABELS.contains(&label.key());
    if !key.labels().any(peer) {
        return None;
    }
    let kept: Vec<Label> = key.labels().filter(|label| !peer(label)).cloned().collect();
    Some(Key::from_parts(key.name().to_owned(), kept))
}

impl<R: Recorder> Recorder for WithoutPeerIdentity<R> {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_counter(key, unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_gauge(key, unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_histogram(key, unit, description);
    }

    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        self.0.register_counter(&without_peer(key).unwrap_or_else(|| key.clone()), metadata)
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        match without_peer(key) {
            Some(_) => Gauge::noop(),
            None => self.0.register_gauge(key, metadata),
        }
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        self.0.register_histogram(&without_peer(key).unwrap_or_else(|| key.clone()), metadata)
    }
}

/// One scrape: `GrpcSnapshot` gauges + process sample set, rendered (blocking: walks every handle)
pub(crate) fn scrape(handle: &PrometheusHandle, status: &DaemonStatus, format: Format) -> Vec<u8> {
    status.emit_gauges();
    collect_process_metrics();
    match format {
        Format::Text => handle.render().into_bytes(),
        Format::Protobuf => handle.render_protobuf(),
    }
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
    zaino_grpc_snapshot::describe_metrics();
    zaino_sync::describe_metrics();
    zaino_persistence::describe_metrics();
    zaino_peers::describe_metrics();
    crate::status::describe_metrics();
    crate::disk_monitor::describe_metrics();
    metrics::describe_gauge!(BUILD_INFO, "Always 1; the zainod version rides the `version` label");
    metrics::gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
}

#[cfg(test)]
mod tests {
    /// zebra-network's emissions (its own label shapes) through zainod's recorder:
    ///
    /// - counters keep every other label, peers summed (2 addresses → one total per command)
    /// - per-peer gauges gone, aggregate gauges + zaino's own labels untouched
    /// - no peer address or user agent anywhere in the scrape
    #[test]
    fn zebra_network_series_carry_no_peer_identity() {
        let (recorder, rendered) = super::recorder().expect("native config is valid");
        metrics::with_local_recorder(&recorder, || {
            for addr in ["203.0.113.7:8233", "198.51.100.2:8233"] {
                metrics::counter!("zcash.net.out.messages", "command" => "ping", "addr" => addr)
                    .increment(2);
                metrics::counter!(
                    "zcash.net.peers.connected",
                    "remote_ip" => addr,
                    "remote_version" => "170150",
                    "negotiated_version" => "170150",
                    "user_agent" => "/Zebra:6.2.3/",
                )
                .increment(1);
                metrics::gauge!("zcash.net.peers.version.connected", "remote_ip" => addr)
                    .set(170150.0);
                metrics::gauge!("zebra.net.connection.state", "command" => "AwaitingRequest", "addr" => addr)
                    .increment(1.0);
            }
            metrics::counter!("zcash.net.peers.cache", "cache" => "/var/lib/zainod/peers", "remote_ip" => "192.0.2.1:8233")
                .increment(1);
            metrics::gauge!("zcash.net.peers", "network" => "Mainnet").set(2.0);
            metrics::counter!("zaino.test.total", "addr" => "kept: not zebra-network's")
                .increment(1);
        });

        let scrape = rendered.render();
        let missing: Vec<&str> = [
            r#"zcash_net_out_messages{command="ping"} 4"#,
            r#"zcash_net_peers_connected{negotiated_version="170150"} 2"#,
            r#"zcash_net_peers_cache{cache="/var/lib/zainod/peers"} 1"#,
            r#"zcash_net_peers{network="Mainnet"} 2"#,
            r#"zaino_test_total{addr="kept: not zebra-network's"} 1"#,
        ]
        .into_iter()
        .filter(|line| !scrape.lines().any(|scraped| scraped == *line))
        .collect();
        assert!(missing.is_empty(), "missing {missing:?} from:\n{scrape}");
        let leaked = [
            "203.0.113.7",
            "198.51.100.2",
            "192.0.2.1",
            "Zebra:6.2.3",
            "version_connected",
            "connection_state",
        ];
        let leaked: Vec<_> = leaked.into_iter().filter(|needle| scrape.contains(needle)).collect();
        assert!(leaked.is_empty(), "{leaked:?} in:\n{scrape}");
    }
}
