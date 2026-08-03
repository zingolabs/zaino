//! JSON-RPC 2.0 client: HTTP transport, auth, and call orchestration.
//!
//! Composes [`envelope`] (request/response serialization) and
//! [`retry`] (retry policy). This module owns the HTTP side only.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::envelope::{self, ResponseOutcome};
use crate::error::RpcError;
use crate::retry;

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
    /// Max retries on work-queue-full (-1) errors.
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
/// on work-queue exhaustion. Returns raw `serde_json::Value` results —
/// response parsing is the adapter crate's responsibility.
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
        // This is a TLS boundary, so it must install the process-level rustls
        // `CryptoProvider` before building the client. The workspace enables
        // reqwest's `rustls-no-provider` feature, which never auto-selects one,
        // so a client built without this panics with "No provider set" the
        // moment it is constructed. First-install-wins, so an embedder that
        // installed its own provider keeps it (ADR-0006).
        zaino_common::crypto::ensure_default_crypto_provider();

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
    /// Retries on work-queue-full errors (code -1) up to `max_retries`.
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, RpcError> {
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);

            let body = envelope::build_request(method, params.clone(), id);
            let response_bytes = self.send_http(&body).await?;
            let outcome = envelope::parse_response(&response_bytes)?;

            match outcome {
                ResponseOutcome::Success(value) => return Ok(value),
                ResponseOutcome::RpcError { code, message } => {
                    if retry::is_retryable(code) && retry::should_retry(attempt, self.max_retries) {
                        tokio::time::sleep(self.retry_delay).await;
                        continue;
                    }
                    return Err(RpcError::Rpc { code, message });
                }
            }
        }
    }

    /// Send an HTTP POST with the JSON body, return raw response bytes.
    async fn send_http(&self, body: &Value) -> Result<Vec<u8>, RpcError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);

        if let Some((ref user, ref pass)) = self.auth {
            request = request.basic_auth(user, Some(pass));
        }

        let response = request.send().await?;
        let status = response.status().as_u16();

        if !(200..300).contains(&status) {
            return Err(RpcError::Status(status));
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }
}
