//! JSON-RPC 2.0 client with retry and auth.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::error::RpcError;

/// Configuration for [`RpcClient`].
pub struct RpcClientConfig {
    /// RPC endpoint URL.
    pub url: String,
    /// Basic auth credentials (username, password). Optional.
    pub auth: Option<(String, String)>,
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP request timeout.
    pub request_timeout: Duration,
    /// Max retries on "work queue depth exceeded" (-1 from server).
    pub max_retries: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
}

impl Default for RpcClientConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8232".to_string(),
            auth: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
            max_retries: 5,
            retry_delay: Duration::from_millis(500),
        }
    }
}

/// A JSON-RPC 2.0 client.
///
/// Handles HTTP transport, JSON-RPC envelope, authentication, and retry
/// on work-queue exhaustion. Returns raw `serde_json::Value` — response
/// parsing is the adapter crate's responsibility.
pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
    id_counter: AtomicI64,
    max_retries: u32,
    retry_delay: Duration,
}

impl RpcClient {
    /// Create a new client from config.
    pub fn new(config: RpcClientConfig) -> Result<Self, RpcError> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            url: config.url,
            client,
            auth: config.auth,
            id_counter: AtomicI64::new(0),
            max_retries: config.max_retries,
            retry_delay: config.retry_delay,
        })
    }

    /// Make a JSON-RPC call.
    ///
    /// Returns the `result` field from the response as a raw `Value`.
    /// Returns `Err(RpcError::Rpc { .. })` if the server returned an error.
    /// Retries on work-queue-full errors (code -1).
    pub async fn call(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<Value, RpcError> {
        let mut attempts = 0u32;

        loop {
            attempts += 1;
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });

            let mut request = self
                .client
                .post(&self.url)
                .header("Content-Type", "application/json")
                .body(serde_json::to_string(&body)?);

            if let Some((ref user, ref pass)) = self.auth {
                request = request.basic_auth(user, Some(pass));
            }

            let response = request.send().await?;
            let status = response.status().as_u16();

            if !(200..300).contains(&status) {
                return Err(RpcError::Status(status));
            }

            let rpc_response: RpcResponse = response.json().await?;

            if let Some(err) = rpc_response.error {
                // Retry on work-queue-full (code -1).
                if err.code == -1 && attempts <= self.max_retries {
                    tokio::time::sleep(self.retry_delay).await;
                    continue;
                }
                return Err(RpcError::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }

            return rpc_response
                .result
                .ok_or_else(|| RpcError::Rpc {
                    code: -1,
                    message: "null result without error".to_string(),
                });
        }
    }
}

/// Raw JSON-RPC 2.0 response envelope.
#[derive(serde::Deserialize)]
struct RpcResponse {
    #[allow(dead_code)]
    id: Value,
    result: Option<Value>,
    error: Option<RpcErrorObject>,
}

/// JSON-RPC error object.
#[derive(serde::Deserialize)]
struct RpcErrorObject {
    code: i64,
    message: String,
}
