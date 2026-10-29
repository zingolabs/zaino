//! Validator RPC metrics, JSON-RPC and indexer gRPC alike (`validator` = configured address: a
//! bounded label set)

use std::future::Future;
use std::time::Instant;

use metrics::{Label, Unit};
use zaino_primitives::metric::{CallMetrics, CallOutcome, Metric};

use crate::legacy_rpc::VERDICT_CODES;
use crate::FailureMode;

const CALLS: CallMetrics = CallMetrics {
    in_flight: "zaino.validator_rpc.in_flight",
    duration: "zaino.validator_rpc.duration_seconds",
    failures: "zaino.validator_rpc.failures_total",
};
const RECEIVED_BYTES: &str = "zaino.validator_rpc.received_bytes_total";

pub const METRICS: &[Metric] = &[
    Metric::histogram(
        CALLS.duration,
        "Validator round trip (one attempt the validator answered, an error reply included), by \
         validator and method",
    )
    .unit(Unit::Seconds),
    Metric::gauge(CALLS.in_flight, "Validator calls in flight, by validator"),
    Metric::counter(RECEIVED_BYTES, "Validator response bytes, by validator").unit(Unit::Bytes),
    Metric::counter(
        CALLS.failures,
        "Validator calls that settled on an error, by validator, method and failure",
    ),
];

pub(crate) fn received(validator: &str, bytes: usize) {
    let bytes = u64::try_from(bytes).expect("usize <= u64 (64-bit targets only)");
    metrics::counter!(RECEIVED_BYTES, "validator" => validator.to_owned()).increment(bytes);
}

/// Every validator call's one funnel, JSON-RPC and indexer gRPC alike (`method` = the RPC's own
/// name, `failure` = how `call`'s output failed, `None` = an answer)
pub(crate) async fn observed<T>(
    validator: &str,
    method: &str,
    call: impl Future<Output = T>,
    failure: impl FnOnce(&T) -> Option<FailureMode>,
) -> T {
    let validator = Label::new("validator", validator.to_owned());
    let _in_flight = CALLS.in_flight(std::slice::from_ref(&validator));
    let started = Instant::now();
    let settled = call.await;
    let took = started.elapsed();
    let label = failure(&settled).map(|mode| (mode, failure_label(mode)));
    let outcome = match &label {
        None => CallOutcome::Answered(took),
        Some((FailureMode::Connection | FailureMode::Timeout, label)) => {
            CallOutcome::Unanswered(label)
        }
        Some((_, label)) => CallOutcome::Refused(took, label),
    };
    CALLS.settled(&[validator, Label::new("method", method.to_owned())], outcome);
    settled
}

/// Bounded label set (verdict codes, HTTP status classes, tonic's 17 gRPC codes)
///
/// - `rpc_other` = any code no verdict reads (the validator picks any i64)
fn failure_label(mode: FailureMode) -> String {
    match mode {
        FailureMode::Connection => "connection".to_owned(),
        FailureMode::Timeout => "timeout".to_owned(),
        FailureMode::Auth => "auth".to_owned(),
        FailureMode::Parse => "parse".to_owned(),
        FailureMode::HttpStatus(status @ 300..=599) => format!("http_{}xx", status / 100),
        FailureMode::HttpStatus(_) => "http_other".to_owned(),
        FailureMode::RpcError(code) if VERDICT_CODES.contains(&code) => format!("rpc_{code}"),
        FailureMode::RpcError(_) => "rpc_other".to_owned(),
        FailureMode::GrpcStatus(code) => format!("grpc_{}", i32::from(code)),
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
            (GrpcStatus(tonic::Code::Unavailable), "grpc_14"),
        ] {
            assert_eq!(failure_label(mode), label, "{mode:?}");
        }
    }
}
