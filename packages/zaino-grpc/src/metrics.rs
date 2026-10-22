//! Serve-path metrics, one call site per measurement

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use metrics::Unit;
use tonic::Code;
use zaino_primitives::metric::Metric;

use crate::admission::Class;
use crate::labels::{label, slot, CODES, LABELS};
use crate::overload::OverloadCause;
use crate::read_lanes::Lane;
use crate::service::{Method, Reply};
use crate::summary;

const REQUESTS_TOTAL: &str = "zaino.grpc.requests_total";
const FIRST_MESSAGE_SECONDS: &str = "zaino.grpc.first_message_seconds";
const DURATION_SECONDS: &str = "zaino.grpc.duration_seconds";
const STREAM_DURATION_SECONDS: &str = "zaino.grpc.stream_duration_seconds";
const STREAM_MESSAGES: &str = "zaino.grpc.stream_messages";
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
const QUEUE_DELAY_SECONDS: &str = "zaino.grpc.queue_delay_seconds";
const RETRANSMIT_RATIO: &str = "zaino.grpc.retransmit_ratio";
const OVERLOAD_LATCHED: &str = "zaino.grpc.overload_latched";

/// One method's series, registered on first use (the recorder is installed at boot, before the
/// server binds: a handle taken earlier would be a no-op forever)
struct MethodSeries {
    timing: Timing,
    sent: metrics::Counter,
    requests: [std::sync::OnceLock<metrics::Counter>; CODES.len()],
}

/// By the row's [`Reply`]: one reply = its latency; a stream = time to first message (its latency)
/// + lifetime + messages (a stream's duration is how long its client kept it, not a latency)
enum Timing {
    One {
        duration: metrics::Histogram,
    },
    Stream {
        first_message: metrics::Histogram,
        lifetime: metrics::Histogram,
        messages: metrics::Histogram,
    },
}

fn series(method: Option<Method>) -> &'static MethodSeries {
    static SERIES: [std::sync::OnceLock<MethodSeries>; LABELS] =
        [const { std::sync::OnceLock::new() }; LABELS];

    let slot = slot(method);
    SERIES[slot].get_or_init(|| {
        let name = label(slot);
        // an unknown path = no row: answered UNIMPLEMENTED in one reply
        let timing = match method.map_or(Reply::One, |method| method.route().reply) {
            Reply::One => {
                Timing::One { duration: metrics::histogram!(DURATION_SECONDS, "method" => name) }
            }
            Reply::Stream => Timing::Stream {
                first_message: metrics::histogram!(FIRST_MESSAGE_SECONDS, "method" => name),
                lifetime: metrics::histogram!(STREAM_DURATION_SECONDS, "method" => name),
                messages: metrics::histogram!(STREAM_MESSAGES, "method" => name),
            },
        };
        MethodSeries {
            timing,
            sent: metrics::counter!(SENT_BYTES_TOTAL, "method" => name),
            requests: [const { std::sync::OnceLock::new() }; CODES.len()],
        }
    })
}

/// Every metric this crate emits
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
    Metric::histogram(
        STREAM_DURATION_SECONDS,
        "Server-streaming RPC lifetime: admission through trailers or the client leaving, by method",
    )
    .unit(Unit::Seconds),
    Metric::histogram(STREAM_MESSAGES, "Messages sent per server stream, by method"),
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
    Metric::histogram(
        QUEUE_DELAY_SECONDS,
        "Send queueing (RTT over the client network's 10 min base RTT), one per client network \
         every 10 s (its sockets' median)",
    )
    .unit(Unit::Seconds),
    Metric::gauge(
        OVERLOAD_LATCHED,
        "Overload causes latched, by cause (1 = latched, also while detection is off; /readyz names \
         the first only while on)",
    ),
    Metric::histogram(
        RETRANSMIT_RATIO,
        "Share of sent data segments retransmitted, one per client network sending >= 100 \
         segments every 10 s",
    ),
];

/// A connection sent GOAWAY (`reason` = `age` | `idle` | `retired`)
pub(crate) fn connection_retired(reason: &'static str) {
    metrics::counter!(RETIRED_CONNECTIONS_TOTAL, "reason" => reason).increment(1);
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

pub(crate) fn queue_delay(delay: Duration) {
    metrics::histogram!(QUEUE_DELAY_SECONDS).record(delay.as_secs_f64());
}

/// Set at each overload window
pub(crate) fn overload_latched(cause: OverloadCause, held: bool) {
    metrics::gauge!(OVERLOAD_LATCHED, "cause" => cause.label()).set(f64::from(u8::from(held)));
}

pub(crate) fn retransmit_ratio(ratio: f64) {
    metrics::histogram!(RETRANSMIT_RATIO).record(ratio);
}

pub(crate) fn disk_read_waited(lane: Lane, waited: Duration) {
    metrics::histogram!(DISK_READ_WAIT_SECONDS, "lane" => lane.label())
        .record(waited.as_secs_f64());
}

/// A stream's first message (a one-reply RPC's = its whole answer: [`stream_finished`] times it)
pub(crate) fn first_message(method: Option<Method>, elapsed: Duration) {
    if let Timing::Stream { first_message, .. } = &series(method).timing {
        first_message.record(elapsed.as_secs_f64());
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
    messages: u64,
    sent: u64,
) {
    summary::finished(method, code, latency);
    let series = series(method);
    // `Code` = the 17 gRPC codes, discriminants 0 to 16 (both inclusive)
    let at = code as usize;
    series.requests[at]
        .get_or_init(|| {
            let (method, code) = (label(slot(method)), CODES[at]);
            metrics::counter!(REQUESTS_TOTAL, "method" => method, "code" => code)
        })
        .increment(1);
    match &series.timing {
        // latency of answers only: a refusal (UNAVAILABLE held, UNIMPLEMENTED) = µs of no work
        Timing::One { duration } if code == Code::Ok => duration.record(elapsed.as_secs_f64()),
        Timing::One { .. } => {}
        Timing::Stream { lifetime, messages: sent_messages, .. } => {
            lifetime.record(elapsed.as_secs_f64());
            sent_messages.record(messages as f64);
        }
    }
    series.sent.increment(sent);
    SENT_BYTES.fetch_add(sent, Ordering::Relaxed);
}
