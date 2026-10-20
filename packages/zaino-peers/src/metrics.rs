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
        "Peer-set request (ready wait + answer), by request"
    );
    metrics::describe_counter!(
        FAILURES_TOTAL,
        "Peer-set requests that settled on an error, by request and failure (timeout; peer = \
         zebra's error, an inventory `notfound` included)"
    );
}

pub(crate) fn request_settled(
    request: &'static str,
    elapsed: Duration,
    failure: Option<&'static str>,
) {
    metrics::histogram!(DURATION_SECONDS, "request" => request).record(elapsed.as_secs_f64());
    if let Some(failure) = failure {
        metrics::counter!(FAILURES_TOTAL, "request" => request, "failure" => failure).increment(1);
    }
}
