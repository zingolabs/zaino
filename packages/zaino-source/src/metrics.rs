//! Validator RPC metrics, JSON-RPC and indexer gRPC alike (`validator` = configured address: a
//! bounded label set)

use std::time::Duration;

use crate::FailureMode;

const DURATION_SECONDS: &str = "zaino.validator_rpc.duration_seconds";
const IN_FLIGHT: &str = "zaino.validator_rpc.in_flight";
const RECEIVED_BYTES: &str = "zaino.validator_rpc.received_bytes_total";
const FAILURES_TOTAL: &str = "zaino.validator_rpc.failures_total";

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_histogram!(
        DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Validator round trip (one attempt the validator answered, an error reply included), by \
         validator and method"
    );
    metrics::describe_gauge!(IN_FLIGHT, "Validator JSON-RPC calls in flight, by validator");
    metrics::describe_counter!(
        RECEIVED_BYTES,
        metrics::Unit::Bytes,
        "Validator JSON-RPC response bytes, by validator"
    );
    metrics::describe_counter!(
        FAILURES_TOTAL,
        "Validator JSON-RPC calls that settled on an error, by validator, method and failure"
    );
}

/// One call in flight (gauge down on drop, whatever ends the call)
pub(crate) struct InFlight(metrics::Gauge);

impl InFlight {
    pub(crate) fn start(validator: &str) -> Self {
        let gauge = metrics::gauge!(IN_FLIGHT, "validator" => validator.to_owned());
        gauge.increment(1.0);
        Self(gauge)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.decrement(1.0);
    }
}

pub(crate) fn received(validator: &str, bytes: usize) {
    metrics::counter!(RECEIVED_BYTES, "validator" => validator.to_owned()).increment(bytes as u64);
}

/// One settled call, JSON-RPC or indexer gRPC (`method` = the RPC's own name)
///
/// - duration = answered attempts only: no answer (refused connection, timeout) = µs of nothing or
///   the timeout itself, never a round trip (failure counter only)
pub(crate) fn call_settled(
    validator: &str,
    method: &str,
    elapsed: Duration,
    failure: Option<FailureMode>,
) {
    if !matches!(failure, Some(FailureMode::Connection | FailureMode::Timeout)) {
        metrics::histogram!(
            DURATION_SECONDS,
            "validator" => validator.to_owned(),
            "method" => method.to_owned(),
        )
        .record(elapsed.as_secs_f64());
    }
    if let Some(mode) = failure {
        metrics::counter!(
            FAILURES_TOTAL,
            "validator" => validator.to_owned(),
            "method" => method.to_owned(),
            "failure" => failure_label(mode),
        )
        .increment(1);
    }
}

/// Bounded label set (zebra's error codes, HTTP statuses, gRPC codes = small fixed sets)
fn failure_label(mode: FailureMode) -> String {
    match mode {
        FailureMode::Connection => "connection".to_owned(),
        FailureMode::Timeout => "timeout".to_owned(),
        FailureMode::Auth => "auth".to_owned(),
        FailureMode::Parse => "parse".to_owned(),
        FailureMode::HttpStatus(status) => format!("http_{status}"),
        FailureMode::RpcError(code) => format!("rpc_{code}"),
        FailureMode::GrpcStatus(code) => format!("grpc_{code}"),
    }
}
