//! Validator JSON-RPC metrics

use std::time::Duration;

use super::error::RpcError;

const DURATION_SECONDS: &str = "zaino.validator_rpc.duration_seconds";
const FAILURES_TOTAL: &str = "zaino.validator_rpc.failures_total";
const RETRIES_TOTAL: &str = "zaino.validator_rpc.retries_total";

/// `(metric, bucket edges)` for the exporter (ms-scale `getblock` → minutes-scale heavy calls)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[(
    DURATION_SECONDS,
    &[1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 50e-3, 100e-3, 250e-3, 1.0, 5.0, 30.0, 120.0],
)];

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_histogram!(
        DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Validator JSON-RPC call, retries included, by method"
    );
    metrics::describe_counter!(
        FAILURES_TOTAL,
        "Validator JSON-RPC calls that settled on an error, by method and failure"
    );
    metrics::describe_counter!(
        RETRIES_TOTAL,
        "Validator JSON-RPC re-sends after a work-queue-full refusal, by method"
    );
}

pub(super) fn call_settled(method: &str, elapsed: Duration, failure: Option<&RpcError>) {
    {
        metrics::histogram!(DURATION_SECONDS, "method" => method.to_owned())
            .record(elapsed.as_secs_f64());
        if let Some(error) = failure {
            metrics::counter!(
                FAILURES_TOTAL,
                "method" => method.to_owned(),
                "failure" => failure_label(error),
            )
            .increment(1);
        }
    }
}

pub(super) fn retried(method: &str) {
    metrics::counter!(RETRIES_TOTAL, "method" => method.to_owned()).increment(1);
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
