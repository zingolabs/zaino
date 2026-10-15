//! Serve-path metrics, one call site per measurement.
//!
//! Every series is labelled by the `CompactTxStreamer` method and, for the
//! request count, the gRPC status code. Both come from fixed tables, so the
//! label set stays bounded however clients behave: an unknown path is
//! `unknown`, never the path itself. Nothing about the client or the request's
//! arguments is ever a label.

use std::sync::OnceLock;
use std::time::Duration;

use tonic::Code;

const REQUESTS_TOTAL: &str = "zaino.grpc.requests_total";
const FIRST_MESSAGE_SECONDS: &str = "zaino.grpc.first_message_seconds";
const DURATION_SECONDS: &str = "zaino.grpc.duration_seconds";
const STREAM_MESSAGES: &str = "zaino.grpc.stream_messages";
const SENT_BYTES_TOTAL: &str = "zaino.grpc.sent_bytes_total";

/// The gRPC path prefix every `CompactTxStreamer` method is served under.
const SERVICE_PREFIX: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";

/// The `CompactTxStreamer` methods, the `method` label's values. A path naming
/// none of them is labelled `unknown`.
const METHODS: [&str; 20] = [
    "GetLatestBlock",
    "GetBlock",
    "GetBlockNullifiers",
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
    "Ping",
];

/// The `method` label slots: one per [`METHODS`] entry, then `unknown`.
const LABELS: usize = METHODS.len() + 1;

/// A method's label slot, from the request path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Method(usize);

impl Method {
    /// The method a request path names, `unknown` for any other path.
    pub(crate) fn of_path(path: &str) -> Self {
        let slot = path
            .strip_prefix(SERVICE_PREFIX)
            .and_then(|name| METHODS.iter().position(|method| *method == name))
            .unwrap_or(METHODS.len());
        Self(slot)
    }

    fn label(self) -> &'static str {
        METHODS.get(self.0).copied().unwrap_or("unknown")
    }
}

/// `Code`'s `Debug` spelling by value: the `code` label, never formatted per
/// request.
const CODES: [&str; 17] = [
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

/// [`CODES`]' slot for `Unknown`.
const UNKNOWN_CODE: usize = 2;

/// One method's series, registered on first use. The recorder is installed at
/// boot, before the server binds, so a handle taken here is never the no-op one.
struct MethodSeries {
    first_message: metrics::Histogram,
    duration: metrics::Histogram,
    messages: metrics::Histogram,
    sent: metrics::Counter,
    requests: [OnceLock<metrics::Counter>; CODES.len()],
}

fn series(method: Method) -> &'static MethodSeries {
    static SERIES: [OnceLock<MethodSeries>; LABELS] = [const { OnceLock::new() }; LABELS];
    let slot = method.0.min(LABELS - 1);
    SERIES[slot].get_or_init(|| {
        let name = method.label();
        MethodSeries {
            first_message: metrics::histogram!(FIRST_MESSAGE_SECONDS, "method" => name),
            duration: metrics::histogram!(DURATION_SECONDS, "method" => name),
            messages: metrics::histogram!(STREAM_MESSAGES, "method" => name),
            sent: metrics::counter!(SENT_BYTES_TOTAL, "method" => name),
            requests: [const { OnceLock::new() }; CODES.len()],
        }
    })
}

/// `(metric, bucket edges)` for the exporter. Each spans what the measurement
/// actually ranges over: a cache hit is microseconds, a cold read tens of
/// milliseconds, a full-chain stream minutes.
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        FIRST_MESSAGE_SECONDS,
        &[
            50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 1.0, 10.0,
        ],
    ),
    (
        DURATION_SECONDS,
        &[1e-3, 10e-3, 100e-3, 1.0, 10.0, 60.0, 300.0, 1800.0],
    ),
    (STREAM_MESSAGES, &[1.0, 10.0, 100.0, 1e3, 10e3, 100e3, 1e6]),
];

/// `# HELP` registrations for every metric this crate emits.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_histogram, Unit};

    describe_counter!(
        REQUESTS_TOTAL,
        "Finished gRPC calls, by method and status code"
    );
    describe_histogram!(
        FIRST_MESSAGE_SECONDS,
        Unit::Seconds,
        "Request received to first response message, by method"
    );
    describe_histogram!(
        DURATION_SECONDS,
        Unit::Seconds,
        "Request received to the response's end, by method"
    );
    describe_histogram!(
        STREAM_MESSAGES,
        "Response messages sent per call, by method"
    );
    describe_counter!(
        SENT_BYTES_TOTAL,
        Unit::Bytes,
        "Response body bytes sent, by method"
    );
}

/// A call's first response message went out `elapsed` after it arrived.
pub(crate) fn first_message(method: Method, elapsed: Duration) {
    series(method).first_message.record(elapsed.as_secs_f64());
}

/// A call's close-out, recorded where its response body drops.
pub(crate) fn call_finished(
    method: Method,
    code: Code,
    elapsed: Duration,
    messages: u64,
    sent: u64,
) {
    let series = series(method);
    // `Code`'s discriminants are the 17 gRPC codes, 0 through 16; anything else
    // would be a code tonic does not define, counted as `Unknown`.
    let at = usize::try_from(i32::from(code))
        .ok()
        .filter(|at| *at < CODES.len())
        .unwrap_or(UNKNOWN_CODE);
    series.requests[at]
        .get_or_init(
            || metrics::counter!(REQUESTS_TOTAL, "method" => method.label(), "code" => CODES[at]),
        )
        .increment(1);
    series.duration.record(elapsed.as_secs_f64());
    // A histogram sample is an f64; a count past u32::MAX saturates there.
    series
        .messages
        .record(f64::from(u32::try_from(messages).unwrap_or(u32::MAX)));
    series.sent.increment(sent);
}

#[cfg(test)]
mod tests {
    use super::{Method, METHODS};

    /// A served path maps to its method; any other path is `unknown`.
    #[test]
    fn paths_map_to_a_bounded_label_set() {
        assert_eq!(
            Method::of_path("/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTreeState").label(),
            "GetTreeState"
        );
        assert_eq!(
            Method::of_path("/other.Service/GetTreeState").label(),
            "unknown"
        );
        assert_eq!(
            Method::of_path("/cash.z.wallet.sdk.rpc.CompactTxStreamer/NoSuchMethod").label(),
            "unknown"
        );
        assert!(METHODS.iter().all(|method| !method.is_empty()));
    }
}
