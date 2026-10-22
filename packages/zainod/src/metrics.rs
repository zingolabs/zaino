//! Prometheus recorder + scrape rendering
//!
//! - State gauges (tips, handed, per-index durable + synced, `zaino_chainview_*`) =
//!   `zaino-grpc-snapshot`'s, set from one `GrpcSnapshot` load per scrape
//!   ([`DaemonStatus::emit_gauges`])
//! - Events emitted where they happen, registered here from each crate's `METRICS` ([`TABLES`])
//! - Every histogram native (exponential buckets, range rescales itself) = protobuf scrapes only
//! - zebra-network's series = a high-level few, no peer identity ([`ZebraNetworkSummary`])

use std::borrow::Cow;

use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Label, Metadata, Recorder, SharedString, Unit,
};
use metrics_exporter_prometheus::{
    Matcher, NativeHistogramConfig, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};
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
    zaino_peers::METRICS,
    crate::status::METRICS,
    crate::disk_monitor::METRICS,
];

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
) -> Result<(ZebraNetworkSummary<PrometheusRecorder>, PrometheusHandle), IndexerError> {
    let native = NativeHistogramConfig::new(BUCKET_FACTOR, MAX_BUCKETS, 0.0)
        .map_err(|e| IndexerError::Metrics(format!("native histogram config: {e}")))?;
    let recorder = PrometheusBuilder::new()
        .set_native_histogram_for_metric(Matcher::Prefix(String::new()), native)
        .build_recorder();
    let handle = recorder.handle();
    Ok((ZebraNetworkSummary(recorder), handle))
}

/// zebra-network's metrics cut to [`ZEBRA_NETWORK_KEPT`], minus who the peer is (its address, or
/// free text it chose: unbounded series, third-party IPs shipped to wherever the scrape goes)
///
/// - kept series: [`PEER_LABELS`] dropped (counters = one total per remaining label set)
/// - every other zebra-network series = noop handle (per-peer debug, no fleet signal)
pub(crate) struct ZebraNetworkSummary<R>(R);

/// zebra-network's metric namespaces (zaino's own = `zaino.` / `process_`)
const ZEBRA_NETWORK: [&str; 6] =
    ["zcash.net.", "zebra.net.", "candidate_set.", "crawler.", "peer.", "pool."];

const ZEBRA_NETWORK_KEPT: [&str; 8] = [
    "zcash.net.peers",
    "zcash.net.in.messages",
    "zcash.net.out.messages",
    "zcash.net.in.bytes.total",
    "zcash.net.out.bytes.total",
    "zcash.net.connection.outcomes.total",
    "pool.num_ready",
    "pool.num_unready",
];

const PEER_LABELS: [&str; 6] =
    ["addr", "remote_ip", "user_agent", "remote_version", "remote_services", "error"];

/// `key` as exported (`None` = dropped zebra-network series)
fn exported(key: &Key) -> Option<Cow<'_, Key>> {
    let name = key.name();
    if !ZEBRA_NETWORK.iter().any(|prefix| name.starts_with(prefix)) {
        return Some(Cow::Borrowed(key));
    }
    if !ZEBRA_NETWORK_KEPT.contains(&name) {
        return None;
    }
    let peer = |label: &Label| PEER_LABELS.contains(&label.key());
    if !key.labels().any(peer) {
        return Some(Cow::Borrowed(key));
    }
    let kept: Vec<Label> = key.labels().filter(|label| !peer(label)).cloned().collect();
    Some(Cow::Owned(Key::from_parts(name.to_owned(), kept)))
}

impl<R: Recorder> Recorder for ZebraNetworkSummary<R> {
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
        exported(key).map_or_else(Counter::noop, |key| self.0.register_counter(&key, metadata))
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        exported(key).map_or_else(Gauge::noop, |key| self.0.register_gauge(&key, metadata))
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        exported(key).map_or_else(Histogram::noop, |key| self.0.register_histogram(&key, metadata))
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
    TABLES.iter().copied().flatten().for_each(Metric::describe);
    metrics::describe_gauge!(BUILD_INFO, "Always 1; the zainod version rides the `version` label");
    metrics::gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
}

#[cfg(test)]
mod tests {
    /// zebra-network's emissions (its own label shapes) through zainod's recorder:
    ///
    /// - kept series keep every other label, peers summed (2 addresses → one total per command)
    /// - every other zebra-network series gone, zaino's own labels untouched
    /// - no peer address or user agent anywhere in the scrape
    #[test]
    fn zebra_network_keeps_its_summary_series_without_peer_identity() {
        let (recorder, rendered) = super::recorder().expect("native config is valid");
        metrics::with_local_recorder(&recorder, || {
            for addr in ["203.0.113.7:8233", "198.51.100.2:8233"] {
                metrics::counter!("zcash.net.out.messages", "command" => "ping", "addr" => addr)
                    .increment(2);
                metrics::counter!("zcash.net.in.messages", "command" => "pong", "addr" => addr)
                    .increment(1);
                metrics::counter!("zcash.net.out.bytes.total", "addr" => addr).increment(100);
                metrics::counter!("zcash.net.in.bytes.total", "addr" => addr).increment(300);
                metrics::counter!(
                    "zcash.net.connection.outcomes.total",
                    "network" => "Mainnet",
                    "direction" => "outbound",
                    "address_family" => "ipv6",
                    "stage" => "handshake",
                    "outcome" => "success",
                )
                .increment(1);
                metrics::counter!(
                    "zcash.net.peers.connected",
                    "remote_ip" => addr,
                    "user_agent" => "/Zebra:6.2.3/",
                )
                .increment(1);
                metrics::gauge!("zcash.net.peers.version.connected", "remote_ip" => addr)
                    .set(170150.0);
                metrics::gauge!("zebra.net.connection.state", "command" => "AwaitingRequest", "addr" => addr)
                    .increment(1.0);
                metrics::histogram!("zcash.net.peer.handshake.duration_seconds", "addr" => addr)
                    .record(0.2);
            }
            metrics::counter!("zcash.net.peers.cache", "remote_ip" => "192.0.2.1:8233")
                .increment(1);
            metrics::gauge!("candidate_set.pending").set(7.0);
            metrics::counter!("peer.canceled").increment(1);
            metrics::gauge!("zcash.net.peers", "network" => "Mainnet").set(2.0);
            metrics::gauge!("pool.num_ready", "network" => "Mainnet").set(2.0);
            metrics::gauge!("pool.num_unready", "network" => "Mainnet").set(0.0);
            metrics::counter!("zaino.test.total", "addr" => "kept: not zebra-network's")
                .increment(1);
        });

        let scrape = rendered.render();
        let missing: Vec<&str> = [
            r#"zcash_net_out_messages{command="ping"} 4"#,
            r#"zcash_net_in_messages{command="pong"} 2"#,
            "zcash_net_out_bytes_total 200",
            "zcash_net_in_bytes_total 600",
            r#"zcash_net_connection_outcomes_total{network="Mainnet",direction="outbound",address_family="ipv6",stage="handshake",outcome="success"} 2"#,
            r#"zcash_net_peers{network="Mainnet"} 2"#,
            r#"pool_num_ready{network="Mainnet"} 2"#,
            r#"pool_num_unready{network="Mainnet"} 0"#,
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
            "zcash_net_peers_",
            "zebra_net",
            "handshake_duration",
            "candidate_set",
            "peer_canceled",
        ];
        let leaked: Vec<_> = leaked.into_iter().filter(|needle| scrape.contains(needle)).collect();
        assert!(leaked.is_empty(), "{leaked:?} in:\n{scrape}");
    }
}
