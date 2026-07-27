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
/// Returns the `result` field on success. Returns an `RpcError` if
/// the response contains an error object or a null result.
pub(crate) fn parse_response(body: &[u8]) -> Result<ResponseOutcome, RpcError> {
    let envelope: RpcResponseEnvelope = serde_json::from_slice(body).map_err(RpcError::Json)?;

    if let Some(err) = envelope.error {
        return Ok(ResponseOutcome::RpcError {
            code: err.code,
            message: err.message,
        });
    }

    match envelope.result {
        Some(value) => Ok(ResponseOutcome::Success(value)),
        None => Err(RpcError::NullResult),
    }
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

    #[test]
    fn parse_null_result_without_error_is_err() {
        let body = br#"{"id":1,"jsonrpc":"2.0","result":null}"#;
        assert!(matches!(parse_response(body), Err(RpcError::NullResult)));
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
