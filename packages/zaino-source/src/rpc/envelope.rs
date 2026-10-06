//! JSON-RPC 2.0 request/response envelope.
//!
//! Pure data: no IO, no HTTP, no retry. Testable in isolation.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::RpcError;

/// Build a JSON-RPC 2.0 request body.
pub(crate) fn build_request(method: &str, params: Vec<Value>, id: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// JSON-RPC 2.0 response body → `result` as `T` (straight off the body bytes, no `Value` hop)
///
/// - Error object → [`ResponseOutcome::RpcError`]; null result → [`RpcError::NullResult`]
pub(crate) fn parse_response<T: DeserializeOwned>(
    body: &[u8],
) -> Result<ResponseOutcome<T>, RpcError> {
    let envelope: RpcResponseEnvelope<T> = serde_json::from_slice(body).map_err(RpcError::Json)?;

    if let Some(err) = envelope.error {
        return Ok(ResponseOutcome::RpcError { code: err.code, message: err.message });
    }

    match envelope.result {
        Some(value) => Ok(ResponseOutcome::Success(value)),
        None => Err(RpcError::NullResult),
    }
}

/// The outcome of parsing a JSON-RPC response envelope.
pub(crate) enum ResponseOutcome<T> {
    /// The server returned a result.
    Success(T),
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
struct RpcResponseEnvelope<T> {
    result: Option<T>,
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

    /// An error object wins over `result: null` (zebrad sends both); `null` alone is a fault,
    /// never an empty answer; an unparseable body is a fault
    #[test]
    fn a_reply_is_its_result_its_error_object_or_a_fault() {
        let parse = |body: &[u8]| match parse_response::<Value>(body) {
            Ok(ResponseOutcome::Success(value)) => format!("ok {value}"),
            Ok(ResponseOutcome::RpcError { code, message }) => format!("rpc {code} {message}"),
            Err(RpcError::NullResult) => "null".to_owned(),
            Err(RpcError::Json(_)) => "json".to_owned(),
            Err(other) => format!("{other:?}"),
        };
        let cases: [(&[u8], &str); 4] = [
            (br#"{"id":1,"result":"ab"}"#, r#"ok "ab""#),
            (br#"{"id":1,"result":null,"error":{"code":-8,"message":"gone"}}"#, "rpc -8 gone"),
            (br#"{"id":1,"result":null}"#, "null"),
            (b"not json", "json"),
        ];
        for (body, expected) in cases {
            assert_eq!(parse(body), expected, "{}", String::from_utf8_lossy(body));
        }
    }
}
