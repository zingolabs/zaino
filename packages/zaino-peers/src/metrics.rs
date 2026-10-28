//! Peer-set request metrics (`request` = zebra's `Request::command`: a bounded label set, the one
//! zebra's own `zebra_net_out_requests` counts by)

use std::time::Duration;

use metrics::Unit;
use zaino_primitives::metric::Metric;

const DURATION_SECONDS: &str = "zaino.peers.request_duration_seconds";
const FAILURES_TOTAL: &str = "zaino.peers.request_failures_total";

pub const METRICS: &[Metric] = &[
    Metric::histogram(
        DURATION_SECONDS,
        "Peer round trip (sent to a ready peer → its answer), answered requests only, by request",
    )
    .unit(Unit::Seconds),
    Metric::counter(
        FAILURES_TOTAL,
        "Peer-set requests that settled on an error, by request and failure (timeout; peer = \
         zebra's error: no ready peer, an inventory `notfound`)",
    ),
];

pub(crate) fn answered(request: &'static str, round_trip: Duration) {
    metrics::histogram!(DURATION_SECONDS, "request" => request).record(round_trip.as_secs_f64());
}

pub(crate) fn failed(request: &'static str, failure: &'static str) {
    metrics::counter!(FAILURES_TOTAL, "request" => request, "failure" => failure).increment(1);
}
