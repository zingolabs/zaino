//! A raw JSON-RPC client for asking the validator directly.
//!
//! The live tests compare Zaino's answer against the validator's own. That
//! comparison is only meaningful if the two sides are reached independently, so
//! this goes straight to the validator over [`zaino_rpc`] and hands back the
//! JSON it sent — no Zaino type is involved in producing it.
//!
//! Answers are `serde_json::Value`. Deserializing them through a Zaino type
//! would make the oracle agree with Zaino by construction on exactly the
//! questions these tests exist to ask: field names, encodings, byte order.
//! Comparing rendered JSON on both sides tests what actually goes on the wire.

use serde_json::Value;
use zaino_rpc::{RpcClient, RpcClientConfig};

/// A direct line to the validator's JSON-RPC interface.
pub struct ValidatorOracle {
    client: RpcClient,
}

impl ValidatorOracle {
    /// Connects to a validator using the regtest test cookie credentials.
    pub fn new(rpc_address: &str) -> Self {
        Self {
            client: RpcClient::new(RpcClientConfig {
                url: format!("http://{rpc_address}"),
                auth: Some(("xxxxxx".to_string(), "xxxxxx".to_string())),
                ..Default::default()
            })
            .expect("the oracle's RPC client is built from a fixed valid config"),
        }
    }

    /// Calls `method` and returns the validator's raw result.
    ///
    /// Panics on failure: an oracle that cannot reach the validator has nothing
    /// to compare against, so the test has already failed.
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Value {
        self.client
            .call(method, params)
            .await
            .unwrap_or_else(|error| panic!("validator oracle call to {method} failed: {error}"))
    }

    /// Calls `method` with no parameters.
    pub async fn get(&self, method: &str) -> Value {
        self.call(method, Vec::new()).await
    }
}
