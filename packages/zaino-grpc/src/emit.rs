//! Serve-path metrics, one call site per measurement

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tonic::Code;

use crate::admission::Class;
use crate::report;

const REQUESTS_TOTAL: &str = "zaino.grpc.requests_total";
const FIRST_MESSAGE_SECONDS: &str = "zaino.grpc.first_message_seconds";
const DURATION_SECONDS: &str = "zaino.grpc.duration_seconds";
const STREAM_MESSAGES: &str = "zaino.grpc.stream_messages";
const SENT_BYTES_TOTAL: &str = "zaino.grpc.sent_bytes_total";
const ACTIVE_STREAMS: &str = "zaino.grpc.active_streams";
const ADMISSION_REJECTED_TOTAL: &str = "zaino.grpc.admission_rejected_total";
const ACTIVE_SUBSCRIPTIONS: &str = "zaino.grpc.active_subscriptions";
const SUBSCRIPTIONS_REJECTED_TOTAL: &str = "zaino.grpc.subscriptions_rejected_total";
const ACCEPT_ERRORS_TOTAL: &str = "zaino.grpc.accept_errors_total";

/// Σ `SENT_BYTES_TOTAL` over methods, readable without the metrics recorder (`/statusz`)
static SENT_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn sent_bytes_total() -> u64 {
    SENT_BYTES.load(Ordering::Relaxed)
}
const DISK_READ_WAIT_SECONDS: &str = "zaino.grpc.disk_read_wait_seconds";
const CONNECTIONS_ACTIVE: &str = "zaino.grpc.connections_active";
const CONNECTIONS_REJECTED_TOTAL: &str = "zaino.grpc.connections_rejected_total";
const STALLED_CONNECTIONS_TOTAL: &str = "zaino.grpc.stalled_connections_total";

/// Every `CompactTxStreamer` method, then `unknown` (any other path): the `method` label set
pub(crate) const METHODS: [&str; 19] = [
    "GetLatestBlock",
    "GetBlock",
    "GetBlockRange",
    "GetBlockRangeNullifiers",
    "GetTransaction",
    "SendTransaction",
    "GetTaddressTxids",
    "GetTaddressTransactions",
    "GetTaddressBalance",
    "GetTaddressBalanceStream",
    "GetMempoolTx",
    "GetMempoolStream",
    "GetTreeState",
    "GetLatestTreeState",
    "GetSubtreeRoots",
    "GetAddressUtxos",
    "GetAddressUtxosStream",
    "GetLightdInfo",
    "unknown",
];

/// A request's slot in [`METHODS`], resolved once from its path (no allocation)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Method(usize);

impl Method {
    /// `/{service}/{method}` → its slot, `unknown` for any other path
    pub(crate) fn of(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or_default();
        let unknown = METHODS.len() - 1;
        Self(METHODS[..unknown].iter().position(|known| *known == name).unwrap_or(unknown))
    }

    pub(crate) fn name(self) -> &'static str {
        METHODS[self.0]
    }

    /// Slot in [`METHODS`]
    pub(crate) fn index(self) -> usize {
        self.0
    }

    /// Open until the next block, idle between arrivals: no latency worth timing
    pub(crate) fn is_subscription(self) -> bool {
        self.name() == "GetMempoolStream"
    }
}

/// `Code`'s `Debug` spelling by value (the `code` label, never formatted per request)
pub(crate) const CODES: [&str; 17] = [
    "Ok",
    "Cancelled",
    "Unknown",
    "InvalidArgument",
    "DeadlineExceeded",
    "NotFound",
    "AlreadyExists",
    "PermissionDenied",
    "ResourceExhausted",
    "FailedPrecondition",
    "Aborted",
    "OutOfRange",
    "Unimplemented",
    "Internal",
    "Unavailable",
    "DataLoss",
    "Unauthenticated",
];

/// One method's series, registered on first use (the recorder is installed at boot, before the
/// server binds: a handle taken earlier would be a no-op forever)
struct MethodSeries {
    first_message: metrics::Histogram,
    duration: metrics::Histogram,
    messages: metrics::Histogram,
    sent: metrics::Counter,
    requests: [std::sync::OnceLock<metrics::Counter>; CODES.len()],
}

fn series(method: Method) -> &'static MethodSeries {
    static SERIES: [std::sync::OnceLock<MethodSeries>; METHODS.len()] =
        [const { std::sync::OnceLock::new() }; METHODS.len()];

    SERIES[method.0].get_or_init(|| {
        let name = METHODS[method.0];
        MethodSeries {
            first_message: metrics::histogram!(FIRST_MESSAGE_SECONDS, "method" => name),
            duration: metrics::histogram!(DURATION_SECONDS, "method" => name),
            messages: metrics::histogram!(STREAM_MESSAGES, "method" => name),
            sent: metrics::counter!(SENT_BYTES_TOTAL, "method" => name),
            requests: [const { std::sync::OnceLock::new() }; CODES.len()],
        }
    })
}

/// `(metric, bucket edges)` for the exporter
///
/// - Per metric (defaults span one decade: RAM hit = µs, cold mmap = 10s of ms, stream = minutes)
/// - `1.0` / `10.0` first-message edges: a load test's second-scale SLO judged on zaino's clock
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        FIRST_MESSAGE_SECONDS,
        &[50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 1.0, 10.0],
    ),
    (DURATION_SECONDS, &[1e-3, 10e-3, 100e-3, 1.0, 10.0, 60.0, 300.0, 1800.0]),
    (STREAM_MESSAGES, &[1.0, 10.0, 100.0, 1e3, 10e3, 100e3, 1e6]),
    (DISK_READ_WAIT_SECONDS, &[1e-6, 100e-6, 1e-3, 10e-3, 100e-3, 1.0, 5.0]),
];

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_counter!(REQUESTS_TOTAL, "Finished streams, by method and gRPC status code");
    describe_histogram!(
        FIRST_MESSAGE_SECONDS,
        Unit::Seconds,
        "Admission to first DATA frame, by method"
    );
    describe_histogram!(DURATION_SECONDS, Unit::Seconds, "Admission through trailers, by method");
    describe_histogram!(STREAM_MESSAGES, "Messages sent per stream, by method");
    describe_counter!(SENT_BYTES_TOTAL, Unit::Bytes, "Response body bytes, by method");
    describe_gauge!(ACTIVE_STREAMS, "Streams holding an admission permit");
    describe_counter!(
        ADMISSION_REJECTED_TOTAL,
        "Streams refused UNAVAILABLE for want of an admission permit"
    );
    describe_gauge!(ACTIVE_SUBSCRIPTIONS, "GetMempoolStream subscriptions holding a permit");
    describe_counter!(
        SUBSCRIPTIONS_REJECTED_TOTAL,
        "Subscriptions refused UNAVAILABLE for want of a subscription permit"
    );
    describe_counter!(
        ACCEPT_ERRORS_TOTAL,
        "accept() failures the listener backed off from (EMFILE, ENFILE, ENOBUFS, ...)"
    );
    describe_histogram!(
        DISK_READ_WAIT_SECONDS,
        Unit::Seconds,
        "Wait for an index-read permit, by lane (point, range, scan)"
    );
    describe_gauge!(CONNECTIONS_ACTIVE, "Open client connections");
    describe_counter!(
        STALLED_CONNECTIONS_TOTAL,
        "Connections closed for holding data their client did not read past the stall timeout"
    );
    describe_counter!(CONNECTIONS_REJECTED_TOTAL, "Connections refused by the accept-time caps");
}

pub(crate) fn connection_opened() {
    metrics::gauge!(CONNECTIONS_ACTIVE).increment(1.0);
}

pub(crate) fn connection_closed() {
    metrics::gauge!(CONNECTIONS_ACTIVE).decrement(1.0);
}

/// A connection closed for holding unread data past the stall timeout
pub(crate) fn connection_stalled() {
    metrics::counter!(STALLED_CONNECTIONS_TOTAL).increment(1);
    report::connection_stalled();
}

pub(crate) fn accept_failed() {
    metrics::counter!(ACCEPT_ERRORS_TOTAL).increment(1);
}

pub(crate) fn connection_rejected() {
    metrics::counter!(CONNECTIONS_REJECTED_TOTAL).increment(1);
    report::connection_refused();
}

fn active(class: Class) -> &'static str {
    match class {
        Class::Work => ACTIVE_STREAMS,
        Class::Subscription => ACTIVE_SUBSCRIPTIONS,
    }
}

pub(crate) fn stream_admitted(class: Class) {
    metrics::gauge!(active(class)).increment(1.0);
}

pub(crate) fn stream_released(class: Class) {
    metrics::gauge!(active(class)).decrement(1.0);
}

pub(crate) fn stream_rejected(class: Class) {
    report::at_capacity();
    match class {
        Class::Work => metrics::counter!(ADMISSION_REJECTED_TOTAL).increment(1),
        Class::Subscription => metrics::counter!(SUBSCRIPTIONS_REJECTED_TOTAL).increment(1),
    }
}

pub(crate) fn disk_read_waited(lane: crate::limits::Lane, waited: Duration) {
    metrics::histogram!(DISK_READ_WAIT_SECONDS, "lane" => lane.label())
        .record(waited.as_secs_f64());
}

pub(crate) fn first_message(method: Method, elapsed: Duration) {
    series(method).first_message.record(elapsed.as_secs_f64());
}

/// One stream's close-out (recorded where the body drops); `latency` = time to its first message,
/// for an answered work request
pub(crate) fn stream_finished(
    method: Method,
    code: Code,
    elapsed: Duration,
    latency: Option<Duration>,
    messages: u64,
    sent: u64,
) {
    report::finished(method, code, latency, sent);
    {
        let series = series(method);
        // `Code` = the 17 gRPC codes, discriminants 0 to 16 (both inclusive)
        let at = code as usize;
        series.requests[at]
            .get_or_init(|| {
                let (method, code) = (METHODS[method.0], CODES[at]);
                metrics::counter!(REQUESTS_TOTAL, "method" => method, "code" => code)
            })
            .increment(1);
        series.duration.record(elapsed.as_secs_f64());
        series.messages.record(messages as f64);
        series.sent.increment(sent);
    }
    SENT_BYTES.fetch_add(sent, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::Method;

    /// Every served path names its own method; any other path (reflection, garbage) shares one
    /// `unknown` label, so the label set stays fixed however clients misbehave
    #[test]
    fn a_path_resolves_to_its_method_or_to_unknown() {
        let service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer";
        for method in super::METHODS.iter().filter(|name| **name != "unknown") {
            assert_eq!(Method::of(&format!("{service}/{method}")).name(), *method);
        }
        for other in ["/grpc.reflection.v1.ServerReflection/Info", "/", "", "/x/GetBlocks"] {
            assert_eq!(Method::of(other).name(), "unknown", "{other:?}");
        }
    }
}
