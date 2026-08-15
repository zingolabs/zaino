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

/// Request timeout for the few JSON-RPC methods that are inherently heavy on the
/// validator, overriding [`RpcClientConfig::request_timeout`].
///
/// `getrawmempool verbose` is the motivating case: Zebra services it by loading
/// full transactions and aggregating descendant stats over the whole mempool, so
/// on a busy chain it legitimately takes longer than the default. Failing it
/// leaves a consumer's mempool marked incomplete precisely when the chain is
/// busiest, so a slow answer is far better than an error.
///
/// This **must** stay above [`RpcClientConfig::request_timeout`]'s default, or
/// the override buys nothing and the failure above happens anyway. Four times
/// the default: enough that a heavy call has real room, bounded enough that an
/// unreachable validator is still noticed in seconds rather than minutes. The
/// caller that waits on it is a poll loop which serves its previous set
/// meanwhile, so a slow answer costs staleness, not availability.
pub const HEAVY_METHOD_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum response body this client will buffer, in bytes.
///
/// Every response is deserialized into memory, so without a cap a validator that
/// is compromised, misconfigured, or simply impersonated can drive the process
/// out of memory with one reply — and the largest legitimate responses (a full
/// block, a verbose mempool listing) are orders of magnitude below this, so the
/// cap has generous headroom before it can affect healthy operation.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

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
        self.call_with_timeout(method, params, None).await
    }

    /// [`Self::call`] with an optional per-request timeout overriding
    /// [`RpcClientConfig::request_timeout`].
    ///
    /// The client-wide timeout suits the small, fast RPCs that dominate normal
    /// traffic, but a few methods are inherently heavy on the validator (see
    /// [`HEAVY_METHOD_TIMEOUT`]). Timing those out turns a slow-but-healthy
    /// validator into a hard error. Overriding per request keeps the tight
    /// default everywhere else.
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);

            let body = envelope::build_request(method, params.clone(), id);
            let response_bytes = self.send_http(&body, timeout).await?;
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
    async fn send_http(
        &self,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>, RpcError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);

        if let Some((ref user, ref pass)) = self.auth {
            request = request.basic_auth(user, Some(pass));
        }

        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }

        let response = request.send().await?;
        let status = response.status().as_u16();

        if !(200..300).contains(&status) {
            return Err(RpcError::Status(status));
        }

        read_body_capped(response, MAX_RESPONSE_BYTES).await
    }
}

/// Reads a response body into memory, abandoning it if it exceeds `max`.
///
/// Chunk-wise rather than [`reqwest::Response::bytes`], which would buffer the
/// whole body before any size could be checked — the point is to never allocate
/// the oversized body in the first place.
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, RpcError> {
    // A truthful Content-Length lets us reject before reading a single chunk;
    // an absent or lying one is caught by the running total below.
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(RpcError::ResponseBodyTooLarge { max });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(RpcError::ResponseBodyTooLarge { max });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of [`HEAVY_METHOD_TIMEOUT`] is headroom over the
    /// client-wide default. Equal values make the override a no-op while
    /// reading, at every call site, as though heavy methods were handled — the
    /// state this constant shipped in before it was caught in review.
    #[test]
    fn heavy_method_timeout_actually_grants_headroom() {
        assert!(
            HEAVY_METHOD_TIMEOUT > RpcClientConfig::default().request_timeout,
            "HEAVY_METHOD_TIMEOUT ({HEAVY_METHOD_TIMEOUT:?}) must exceed the default \
             request_timeout ({:?}), or overriding with it changes nothing",
            RpcClientConfig::default().request_timeout,
        );
    }
}
