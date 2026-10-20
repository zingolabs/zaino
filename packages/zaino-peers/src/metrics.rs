//! Peer-set request metrics (`request` = zebra's `Request::command`: a bounded label set, the one
//! zebra's own `zebra_net_out_requests` counts by)

use std::time::Duration;

const DURATION_SECONDS: &str = "zaino.peers.request_duration_seconds";
const FAILURES_TOTAL: &str = "zaino.peers.request_failures_total";

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_histogram!(
        DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Peer round trip (sent to a ready peer → its answer), answered requests only, by request"
    );
    metrics::describe_counter!(
        FAILURES_TOTAL,
        "Peer-set requests that settled on an error, by request and failure (timeout; peer = \
         zebra's error: no ready peer, an inventory `notfound`)"
    );
}

pub(crate) fn answered(request: &'static str, round_trip: Duration) {
    metrics::histogram!(DURATION_SECONDS, "request" => request).record(round_trip.as_secs_f64());
}

pub(crate) fn failed(request: &'static str, failure: &'static str) {
    metrics::counter!(FAILURES_TOTAL, "request" => request, "failure" => failure).increment(1);
}
