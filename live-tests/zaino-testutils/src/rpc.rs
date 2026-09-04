//! JSON-RPC parity helper shared by the parity tests.

use anyhow::Result;
use serde_json::Value;
use ztest::prelude::JsonRpcClient;

use crate::json::json_equal_ignoring;

/// Call `method(params)` on two JSON-RPC sources and assert the responses
/// agree, after dropping `volatile` (dot-separated) paths. Returns the
/// first source's response so the caller can pluck fields from it for
/// follow-up assertions.
///
/// Folds the `call`/`call`/`assert_eq!` triple duplicated across the
/// fetch/state/json_server parity tests into one call. `method` doubles
/// as the failure label. (Array responses needing order normalization —
/// e.g. `getrawmempool` — are compared inline at the call site instead.)
/// A mismatch is returned as `Err`, not panicked: see the module docs on
/// `json.rs` for why the composable form is the one that exists here.
pub async fn assert_rpc_parity(
    method: &'static str,
    params: &str,
    a: &JsonRpcClient,
    b: &JsonRpcClient,
    volatile: &[&str],
) -> Result<Value> {
    let params: Value = if params.is_empty() {
        Value::Array(Vec::new())
    } else {
        serde_json::from_str(params)?
    };
    let av = a.call_value(method, params.clone()).await?;
    let bv = b.call_value(method, params).await?;
    json_equal_ignoring(method, av.clone(), bv, volatile)?;
    Ok(av)
}
