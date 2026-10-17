//! JSON-RPC 2.0 request/response envelope.
//!
//! Pure data: no IO, no HTTP, no retry. Testable in isolation.

use serde_json::Value;

use crate::error::RpcError;

/// Build a JSON-RPC 2.0 request body.
pub(crate) fn build_request(method: &str, params: Vec<Value>, id: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Parse a JSON-RPC 2.0 response body.
///
/// Returns an `RpcError` if the response contains an error object. Otherwise
/// returns the `result` field as the success value — including when it is a
/// literal JSON `null`, which several methods (`ping` among them) use as
/// their only successful answer. Serde's `Option<Value>` cannot tell a
/// present-but-null `result` apart from an absent one, so this reads success
/// purely from the absence of `error` rather than the presence of `result`;
/// a response missing both is treated the same as one with an explicit null.
pub(crate) fn parse_response(body: &[u8]) -> Result<ResponseOutcome, RpcError> {
    let envelope: RpcResponseEnvelope = serde_json::from_slice(body).map_err(RpcError::Json)?;

    if let Some(err) = envelope.error {
        return Ok(ResponseOutcome::RpcError {
            code: err.code,
            message: err.message,
        });
    }

    Ok(ResponseOutcome::Success(
        envelope.result.unwrap_or(Value::Null),
    ))
}

/// The outcome of parsing a JSON-RPC response envelope.
pub(crate) enum ResponseOutcome {
    /// The server returned a result.
    Success(Value),
    /// The server returned a JSON-RPC error object.
    RpcError {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
}

/// Raw JSON-RPC 2.0 response envelope.
#[derive(serde::Deserialize)]
struct RpcResponseEnvelope {
    #[allow(dead_code)]
    id: Value,
    result: Option<Value>,
    error: Option<RpcErrorObject>,
}

/// JSON-RPC error object within the envelope.
#[derive(serde::Deserialize)]
struct RpcErrorObject {
    code: i64,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_has_correct_shape() {
        let req = build_request("getblock", vec![Value::from("100"), Value::from(0)], 42);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 42);
        assert_eq!(req["method"], "getblock");
        assert_eq!(req["params"][0], "100");
        assert_eq!(req["params"][1], 0);
    }

    #[test]
    fn parse_success_response() {
        let body = br#"{"id":1,"jsonrpc":"2.0","result":"deadbeef"}"#;
        match parse_response(body).expect("valid") {
            ResponseOutcome::Success(v) => assert_eq!(v, "deadbeef"),
            ResponseOutcome::RpcError { .. } => panic!("expected success"),
        }
    }

    #[test]
    fn parse_error_response() {
        let body = br#"{"id":1,"jsonrpc":"2.0","result":null,"error":{"code":-8,"message":"Block not found"}}"#;
        match parse_response(body).expect("valid") {
            ResponseOutcome::RpcError { code, message } => {
                assert_eq!(code, -8);
                assert_eq!(message, "Block not found");
            }
            ResponseOutcome::Success(_) => panic!("expected error"),
        }
    }

    /// `ping`'s successful response is exactly this shape: a literal null
    /// `result` and no `error`. It must read as success, not as a failure —
    /// fails if `Option<Value>` is allowed to collapse this into "absent".
    #[test]
    fn parse_null_result_without_error_is_success() {
        let body = br#"{"id":1,"jsonrpc":"2.0","result":null}"#;
        match parse_response(body).expect("valid") {
            ResponseOutcome::Success(v) => assert_eq!(v, Value::Null),
            ResponseOutcome::RpcError { .. } => panic!("expected success"),
        }
    }

    /// A response with neither `result` nor `error` is malformed, but treating
    /// it the same as an explicit null is strictly more lenient than rejecting
    /// it, and this parser has no basis to prefer rejection.
    #[test]
    fn parse_missing_result_without_error_is_success() {
        let body = br#"{"id":1,"jsonrpc":"2.0"}"#;
        match parse_response(body).expect("valid") {
            ResponseOutcome::Success(v) => assert_eq!(v, Value::Null),
            ResponseOutcome::RpcError { .. } => panic!("expected success"),
        }
    }

    #[test]
    fn parse_malformed_json_is_err() {
        let body = b"not json";
        assert!(matches!(parse_response(body), Err(RpcError::Json(_))));
    }

    #[test]
    fn parse_object_result() {
        let body = br#"{"id":1,"jsonrpc":"2.0","result":{"height":100,"hash":"abc"}}"#;
        match parse_response(body).expect("valid") {
            ResponseOutcome::Success(v) => {
                assert_eq!(v["height"], 100);
                assert_eq!(v["hash"], "abc");
            }
            ResponseOutcome::RpcError { .. } => panic!("expected success"),
        }
    }
}
