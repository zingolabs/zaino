//! JSON-RPC 2.0 request/response envelope (pure data: no IO, no HTTP, no retry)

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::client::Call;
use super::error::RpcError;

pub(crate) fn build_request(method: &str, params: &[Value], id: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// One request per call, ids `first..`
pub(crate) fn build_batch<'a>(calls: impl IntoIterator<Item = &'a Call>, first: i64) -> Value {
    let calls = calls
        .into_iter()
        .zip(first..)
        .map(|(call, id)| build_request(call.method, &call.params, id));
    Value::Array(calls.collect())
}

/// JSON-RPC 2.0 response body → `result` as `T` (straight off the body bytes, no `Value` hop)
///
/// - Error object → [`RpcError::Rpc`]; null result → [`RpcError::NullResult`]
pub(crate) fn parse_response<T: DeserializeOwned>(body: &[u8]) -> Result<T, RpcError> {
    serde_json::from_slice::<RpcResponseEnvelope<T>>(body)?.into_result()
}

/// Batch reply → one outcome per call, in call order (server may reorder: matched by `id`)
///
/// - reply missing, duplicated or naming an unsent id → whole batch [`RpcError::BatchMismatch`]
pub(crate) fn parse_batch<T: DeserializeOwned>(
    body: &[u8],
    first: i64,
    len: usize,
) -> Result<Vec<Result<T, RpcError>>, RpcError> {
    let replies: Vec<RpcResponseEnvelope<T>> = serde_json::from_slice(body)?;
    let mut slots: Vec<Option<Result<T, RpcError>>> = (0..len).map(|_| None).collect();
    for reply in replies {
        let index = reply.id.and_then(|id| usize::try_from(id.checked_sub(first)?).ok());
        match index.and_then(|index| slots.get_mut(index)) {
            Some(slot) if slot.is_none() => *slot = Some(reply.into_result()),
            _ => return Err(RpcError::BatchMismatch),
        }
    }
    slots.into_iter().map(|slot| slot.ok_or(RpcError::BatchMismatch)).collect()
}

#[derive(serde::Deserialize)]
struct RpcResponseEnvelope<T> {
    id: Option<i64>,
    result: Option<T>,
    error: Option<RpcErrorObject>,
}

impl<T> RpcResponseEnvelope<T> {
    /// Error object wins over `result: null` (zebrad sends both)
    fn into_result(self) -> Result<T, RpcError> {
        match (self.error, self.result) {
            (Some(RpcErrorObject { code, message }), _) => Err(RpcError::Rpc { code, message }),
            (None, Some(value)) => Ok(value),
            (None, None) => Err(RpcError::NullResult),
        }
    }
}

#[derive(serde::Deserialize)]
struct RpcErrorObject {
    code: i64,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(parsed: Result<Value, RpcError>) -> String {
        match parsed {
            Ok(value) => format!("ok {value}"),
            Err(RpcError::Rpc { code, message }) => format!("rpc {code} {message}"),
            Err(RpcError::NullResult) => "null".to_owned(),
            Err(RpcError::Json(_)) => "json".to_owned(),
            Err(other) => format!("{other:?}"),
        }
    }

    /// - Error object wins over `result: null` (zebrad sends both)
    /// - `null` alone = fault, never an empty answer; unparseable body = fault
    #[test]
    fn a_reply_is_its_result_its_error_object_or_a_fault() {
        let cases: [(&[u8], &str); 4] = [
            (br#"{"id":1,"result":"ab"}"#, r#"ok "ab""#),
            (br#"{"id":1,"result":null,"error":{"code":-8,"message":"gone"}}"#, "rpc -8 gone"),
            (br#"{"id":1,"result":null}"#, "null"),
            (b"not json", "json"),
        ];
        for (body, expected) in cases {
            assert_eq!(
                outcome(parse_response(body)),
                expected,
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// - Replies in call order whatever their arrival order, one outcome per item
    /// - Reply set != exactly the ids sent → whole batch fails
    #[test]
    fn batch_replies_are_matched_by_id_and_must_answer_every_call_once() {
        let calls = [
            Call { method: "getrawtransaction", params: vec!["aa".into()] },
            Call { method: "getinfo", params: vec![] },
        ];
        let batch = build_batch(&calls, 7);
        assert_eq!(batch[0]["id"], 7);
        assert_eq!(batch[0]["params"][0], "aa");
        assert_eq!(
            (&batch[1]["id"], &batch[1]["method"]),
            (&Value::from(8), &Value::from("getinfo"))
        );

        let reordered = br#"[{"id":8,"result":null,"error":{"code":-5,"message":"gone"}},
                             {"id":7,"result":"ab"}]"#;
        let items = parse_batch::<Value>(reordered, 7, 2).expect("well-formed batch");
        assert_eq!(
            items.into_iter().map(outcome).collect::<Vec<_>>(),
            [r#"ok "ab""#, "rpc -5 gone"]
        );

        for broken in [
            &br#"[{"id":7,"result":"ab"}]"#[..],
            br#"[{"id":7,"result":"ab"},{"id":7,"result":"ab"}]"#,
            br#"[{"id":7,"result":"ab"},{"id":9,"result":"ab"}]"#,
            br#"[{"id":6,"result":"ab"},{"id":7,"result":"ab"}]"#,
        ] {
            let parsed = parse_batch::<Value>(broken, 7, 2);
            assert!(
                matches!(parsed, Err(RpcError::BatchMismatch)),
                "{}",
                String::from_utf8_lossy(broken)
            );
        }
        // whole-batch refusal (jsonrpsee answers an invalid batch with one object, not an array)
        let refused = br#"{"id":null,"error":{"code":-32600,"message":"Invalid request"}}"#;
        assert!(matches!(parse_batch::<Value>(refused, 7, 2), Err(RpcError::Json(_))));
    }
}
