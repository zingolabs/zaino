//! Serve-path metrics, one call site per measurement

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use metrics::Unit;
use tonic::Code;
use zaino_primitives::metric::Metric;

use crate::admission::Class;
use crate::lifetime::RetireReason;
use crate::overload::OverloadCause;
use crate::read_lanes::Lane;
use crate::service::{Method, Reply};
use crate::summary;

const REQUESTS_TOTAL: &str = "zaino.grpc.requests_total";
const FIRST_MESSAGE_SECONDS: &str = "zaino.grpc.first_message_seconds";
const DURATION_SECONDS: &str = "zaino.grpc.duration_seconds";
const SENT_BYTES_TOTAL: &str = "zaino.grpc.sent_bytes_total";
const ACTIVE_STREAMS: &str = "zaino.grpc.active_streams";
const ADMISSION_REJECTED_TOTAL: &str = "zaino.grpc.admission_rejected_total";
const ACTIVE_SUBSCRIPTIONS: &str = "zaino.grpc.active_subscriptions";
const SUBSCRIPTIONS_REJECTED_TOTAL: &str = "zaino.grpc.subscriptions_rejected_total";
const ACCEPT_ERRORS_TOTAL: &str = "zaino.grpc.accept_errors_total";

/// Σ `SENT_BYTES_TOTAL` over methods, readable without the metrics recorder (zainod's egress)
static SENT_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn sent_bytes_total() -> u64 {
    SENT_BYTES.load(Ordering::Relaxed)
}
const DISK_READ_WAIT_SECONDS: &str = "zaino.grpc.disk_read_wait_seconds";
const CONNECTIONS_ACTIVE: &str = "zaino.grpc.connections_active";
const CONNECTIONS_REJECTED_TOTAL: &str = "zaino.grpc.connections_rejected_total";
const STALLED_CONNECTIONS_TOTAL: &str = "zaino.grpc.stalled_connections_total";
const RETIRED_CONNECTIONS_TOTAL: &str = "zaino.grpc.retired_connections_total";
const ADAPTIVE_STREAM_LIMIT: &str = "zaino.grpc.adaptive_stream_limit";
const OVERLOAD_LATCHED: &str = "zaino.grpc.overload_latched";

/// `method` label (`None` = `unknown`: any other path, so the set stays fixed)
pub(crate) fn method_label(method: Option<Method>) -> &'static str {
    method.map_or("unknown", Method::name)
}

/// `code` label = `Code`'s `Debug` spelling (never formatted per request)
pub(crate) fn code_label(code: Code) -> &'static str {
    match code {
        Code::Ok => "Ok",
        Code::Cancelled => "Cancelled",
        Code::Unknown => "Unknown",
        Code::InvalidArgument => "InvalidArgument",
        Code::DeadlineExceeded => "DeadlineExceeded",
        Code::NotFound => "NotFound",
        Code::AlreadyExists => "AlreadyExists",
        Code::PermissionDenied => "PermissionDenied",
        Code::ResourceExhausted => "ResourceExhausted",
        Code::FailedPrecondition => "FailedPrecondition",
        Code::Aborted => "Aborted",
        Code::OutOfRange => "OutOfRange",
        Code::Unimplemented => "Unimplemented",
        Code::Internal => "Internal",
        Code::Unavailable => "Unavailable",
        Code::DataLoss => "DataLoss",
        Code::Unauthenticated => "Unauthenticated",
    }
}

/// Unknown path = one reply (answered `UNIMPLEMENTED`)
fn reply(method: Option<Method>) -> Reply {
    method.map_or(Reply::One, |method| method.route().reply)
}

pub const METRICS: &[Metric] = &[
    Metric::counter(REQUESTS_TOTAL, "Finished requests, by method and gRPC status code"),
    Metric::histogram(
        DURATION_SECONDS,
        "One-reply RPC latency: admission through trailers, OK answers only, by method",
    )
    .unit(Unit::Seconds),
    Metric::histogram(
        FIRST_MESSAGE_SECONDS,
        "Server-streaming RPC latency: admission to first message, by method",
    )
    .unit(Unit::Seconds),
    Metric::counter(SENT_BYTES_TOTAL, "Response body bytes, by method").unit(Unit::Bytes),
    Metric::gauge(ACTIVE_STREAMS, "Work streams holding an admission slot"),
    Metric::counter(
        ADMISSION_REJECTED_TOTAL,
        "Work streams refused UNAVAILABLE at the adaptive work-stream limit",
    ),
    Metric::gauge(ACTIVE_SUBSCRIPTIONS, "GetMempoolStream subscriptions holding a slot"),
    Metric::counter(
        SUBSCRIPTIONS_REJECTED_TOTAL,
        "Subscriptions refused UNAVAILABLE at max_subscriptions",
    ),
    Metric::counter(
        ACCEPT_ERRORS_TOTAL,
        "accept() failures the listener backed off from (EMFILE, ENFILE, ENOBUFS, ...)",
    ),
    Metric::histogram(
        DISK_READ_WAIT_SECONDS,
        "Wait for an index-read permit, by lane (point, range, scan)",
    )
    .unit(Unit::Seconds),
    Metric::gauge(CONNECTIONS_ACTIVE, "Open client connections"),
    Metric::counter(
        STALLED_CONNECTIONS_TOTAL,
        "Connections closed for holding data their client did not read within unread_timeout",
    ),
    Metric::counter(CONNECTIONS_REJECTED_TOTAL, "Connections refused by the accept-time caps"),
    Metric::counter(
        RETIRED_CONNECTIONS_TOTAL,
        "Connections sent GOAWAY, by reason: age, idle (gRPC A9) or retired (an operator hold)",
    ),
    Metric::gauge(
        ADAPTIVE_STREAM_LIMIT,
        "Work streams admitted at once now (latency-adaptive; max_work_streams when off)",
    ),
    Metric::gauge(
        OVERLOAD_LATCHED,
        "Overload causes latched, by cause (1 = latched, also while detection is off; /readyz names \
         the first only while on)",
    ),
];

pub(crate) fn connection_retired(reason: RetireReason) {
    metrics::counter!(RETIRED_CONNECTIONS_TOTAL, "reason" => reason.label()).increment(1);
}

/// Set at each serving summary
pub(crate) fn adaptive_stream_limit(limit: usize) {
    metrics::gauge!(ADAPTIVE_STREAM_LIMIT).set(limit as f64);
}

pub(crate) fn connection_opened() {
    metrics::gauge!(CONNECTIONS_ACTIVE).increment(1.0);
}

pub(crate) fn connection_closed() {
    metrics::gauge!(CONNECTIONS_ACTIVE).decrement(1.0);
}

/// A connection closed for holding data unread past `unread_timeout`
pub(crate) fn connection_stalled() {
    metrics::counter!(STALLED_CONNECTIONS_TOTAL).increment(1);
    summary::connection_stalled();
}

pub(crate) fn accept_failed() {
    metrics::counter!(ACCEPT_ERRORS_TOTAL).increment(1);
}

pub(crate) fn connection_rejected() {
    metrics::counter!(CONNECTIONS_REJECTED_TOTAL).increment(1);
    summary::connection_refused();
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
    match class {
        Class::Work => metrics::counter!(ADMISSION_REJECTED_TOTAL).increment(1),
        Class::Subscription => metrics::counter!(SUBSCRIPTIONS_REJECTED_TOTAL).increment(1),
    }
}

/// Set at each overload window
pub(crate) fn overload_latched(cause: OverloadCause, held: bool) {
    metrics::gauge!(OVERLOAD_LATCHED, "cause" => cause.label()).set(f64::from(u8::from(held)));
}

pub(crate) fn disk_read_waited(lane: Lane, waited: Duration) {
    metrics::histogram!(DISK_READ_WAIT_SECONDS, "lane" => lane.label())
        .record(waited.as_secs_f64());
}

/// A stream's first message (a one-reply RPC's = its whole answer: [`stream_finished`] times it)
pub(crate) fn first_message(method: Option<Method>, elapsed: Duration) {
    if reply(method) == Reply::Stream {
        let method = method_label(method);
        metrics::histogram!(FIRST_MESSAGE_SECONDS, "method" => method).record(elapsed);
    }
}

/// One stream's close-out (recorded where the body drops)
///
/// - `latency` = time to its first message, for an answered work request
pub(crate) fn stream_finished(
    method: Option<Method>,
    code: Code,
    elapsed: Duration,
    latency: Option<Duration>,
    sent: u64,
) {
    summary::finished(code, latency);
    let name = method_label(method);
    metrics::counter!(REQUESTS_TOTAL, "method" => name, "code" => code_label(code)).increment(1);
    // answers only (refusal = µs of no work)
    if reply(method) == Reply::One && code == Code::Ok {
        metrics::histogram!(DURATION_SECONDS, "method" => name).record(elapsed);
    }
    metrics::counter!(SENT_BYTES_TOTAL, "method" => name).increment(sent);
    SENT_BYTES.fetch_add(sent, Ordering::Relaxed);
}
