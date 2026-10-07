//! Validator JSON-RPC metrics (`validator` = configured address: a bounded label set)

use std::time::Duration;

use super::client::Lane;
use super::error::RpcError;

const DURATION_SECONDS: &str = "zaino.validator_rpc.duration_seconds";
const WAIT_SECONDS: &str = "zaino.validator_rpc.wait_seconds";
const IN_FLIGHT: &str = "zaino.validator_rpc.in_flight";
const RECEIVED_BYTES: &str = "zaino.validator_rpc.received_bytes_total";
const FAILURES_TOTAL: &str = "zaino.validator_rpc.failures_total";
const RETRIES_TOTAL: &str = "zaino.validator_rpc.retries_total";

/// `(metric, bucket edges)` for the exporter (ms-scale `getblock` → minutes-scale heavy calls)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        DURATION_SECONDS,
        &[1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 50e-3, 100e-3, 250e-3, 1.0, 5.0, 30.0, 120.0],
    ),
    (WAIT_SECONDS, &[1e-4, 1e-3, 10e-3, 100e-3, 1.0, 10.0]),
];

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_histogram!(
        DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Validator JSON-RPC call, retries included, by validator and method"
    );
    metrics::describe_histogram!(
        WAIT_SECONDS,
        metrics::Unit::Seconds,
        "Time a call waited for its lane permit and request budget, by validator and lane"
    );
    metrics::describe_gauge!(
        IN_FLIGHT,
        "Validator JSON-RPC calls in flight, by validator and lane"
    );
    metrics::describe_counter!(
        RECEIVED_BYTES,
        metrics::Unit::Bytes,
        "Validator JSON-RPC response bytes, by validator"
    );
    metrics::describe_counter!(
        FAILURES_TOTAL,
        "Validator JSON-RPC calls that settled on an error, by validator, method and failure"
    );
    metrics::describe_counter!(
        RETRIES_TOTAL,
        "Validator JSON-RPC re-sends after a work-queue-full refusal, by validator and method"
    );
}

pub(super) fn waited(validator: &str, lane: Lane, waited: Duration) {
    metrics::histogram!(WAIT_SECONDS, "validator" => validator.to_owned(), "lane" => lane.label())
        .record(waited.as_secs_f64());
}

/// One call in flight on a lane (gauge down on drop, whatever ends the call)
pub(super) struct InFlight(metrics::Gauge);

impl InFlight {
    pub(super) fn start(validator: &str, lane: Lane) -> Self {
        let gauge =
            metrics::gauge!(IN_FLIGHT, "validator" => validator.to_owned(), "lane" => lane.label());
        gauge.increment(1.0);
        Self(gauge)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.decrement(1.0);
    }
}

pub(super) fn received(validator: &str, bytes: usize) {
    metrics::counter!(RECEIVED_BYTES, "validator" => validator.to_owned()).increment(bytes as u64);
}

pub(super) fn call_settled(
    validator: &str,
    method: &str,
    elapsed: Duration,
    failure: Option<&RpcError>,
) {
    metrics::histogram!(
        DURATION_SECONDS,
        "validator" => validator.to_owned(),
        "method" => method.to_owned(),
    )
    .record(elapsed.as_secs_f64());
    if let Some(error) = failure {
        metrics::counter!(
            FAILURES_TOTAL,
            "validator" => validator.to_owned(),
            "method" => method.to_owned(),
            "failure" => failure_label(error),
        )
        .increment(1);
    }
}

pub(super) fn retried(validator: &str, method: &str) {
    metrics::counter!(
        RETRIES_TOTAL,
        "validator" => validator.to_owned(),
        "method" => method.to_owned(),
    )
    .increment(1);
}

/// Bounded label set (zebra's error codes + HTTP statuses = small fixed sets)
fn failure_label(error: &RpcError) -> String {
    use crate::FailureMode;

    match error.failure_mode() {
        FailureMode::Connection => "connection".to_owned(),
        FailureMode::Timeout => "timeout".to_owned(),
        FailureMode::Auth => "auth".to_owned(),
        FailureMode::Parse => "parse".to_owned(),
        FailureMode::HttpStatus(status) => format!("http_{status}"),
        FailureMode::RpcError(code) => format!("rpc_{code}"),
    }
}
