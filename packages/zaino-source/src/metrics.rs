//! Validator RPC metrics, JSON-RPC and indexer gRPC alike (`validator` = configured address: a
//! bounded label set)

use std::time::Duration;

use metrics::Unit;
use zaino_primitives::metric::Metric;

use crate::FailureMode;

const DURATION_SECONDS: &str = "zaino.validator_rpc.duration_seconds";
const IN_FLIGHT: &str = "zaino.validator_rpc.in_flight";
const RECEIVED_BYTES: &str = "zaino.validator_rpc.received_bytes_total";
const FAILURES_TOTAL: &str = "zaino.validator_rpc.failures_total";

/// Every metric this crate emits
pub const METRICS: &[Metric] = &[
    Metric::histogram(
        DURATION_SECONDS,
        "Validator round trip (one attempt the validator answered, an error reply included), by \
         validator and method",
    )
    .unit(Unit::Seconds),
    Metric::gauge(IN_FLIGHT, "Validator JSON-RPC calls in flight, by validator"),
    Metric::counter(RECEIVED_BYTES, "Validator JSON-RPC response bytes, by validator")
        .unit(Unit::Bytes),
    Metric::counter(
        FAILURES_TOTAL,
        "Validator JSON-RPC calls that settled on an error, by validator, method and failure",
    ),
];

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

/// Codes `legacy_rpc` reads a verdict from (any other = `rpc_other`: the validator picks any i64)
const VERDICT_RPC_CODES: [i64; 9] = [-1, -5, -8, -22, -25, -26, -27, -32601, -32602];

/// Bounded label set (verdict codes, HTTP status classes, tonic's 17 gRPC codes)
fn failure_label(mode: FailureMode) -> String {
    match mode {
        FailureMode::Connection => "connection".to_owned(),
        FailureMode::Timeout => "timeout".to_owned(),
        FailureMode::Auth => "auth".to_owned(),
        FailureMode::Parse => "parse".to_owned(),
        FailureMode::HttpStatus(status @ 300..=599) => format!("http_{}xx", status / 100),
        FailureMode::HttpStatus(_) => "http_other".to_owned(),
        FailureMode::RpcError(code) if VERDICT_RPC_CODES.contains(&code) => format!("rpc_{code}"),
        FailureMode::RpcError(_) => "rpc_other".to_owned(),
        FailureMode::GrpcStatus(code) => format!("grpc_{code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_labels_stay_bounded_whatever_the_validator_sends() {
        use FailureMode::{GrpcStatus, HttpStatus, RpcError};
        for (mode, label) in [
            (RpcError(-8), "rpc_-8"),
            (RpcError(-32601), "rpc_-32601"),
            (RpcError(-28), "rpc_other"),
            (RpcError(i64::MIN), "rpc_other"),
            (HttpStatus(503), "http_5xx"),
            (HttpStatus(429), "http_4xx"),
            (HttpStatus(301), "http_3xx"),
            (HttpStatus(999), "http_other"),
            (GrpcStatus(14), "grpc_14"),
        ] {
            assert_eq!(failure_label(mode), label, "{mode:?}");
        }
    }
}
