//! JSON-RPC 2.0 over HTTP: auth, capped body, work-queue-full retry

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::emit;
use super::envelope::{self, ResponseOutcome};
use super::error::RpcError;

/// Largest body buffered (a hostile or broken validator cannot OOM the process with one reply;
/// the largest real answer, a full block, is far below)
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Zebra's "work queue full": busy, not broken
const WORK_QUEUE_FULL: i64 = -1;

pub struct RpcClientConfig {
    pub url: String,
    /// Basic auth (user, password)
    pub auth: Option<(String, String)>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    /// Re-sends after a work-queue-full refusal
    pub max_retries: u32,
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

pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
    id_counter: AtomicI64,
    max_retries: u32,
    retry_delay: Duration,
}

impl RpcClient {
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

    /// `result` as a raw `Value`
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, RpcError> {
        self.call_as(method, params).await
    }

    /// `result` deserialized straight off the body as `T` (no `Value` hop: a multi-MB hex block
    /// is decoded from the body bytes once)
    pub async fn call_as<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let started = Instant::now();
        let settled = self.send_until_settled(method, params).await;
        emit::call_settled(method, started.elapsed(), settled.as_ref().err());
        settled
    }

    async fn send_until_settled<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let mut retries = 0u32;
        loop {
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
            let body = envelope::build_request(method, params.clone(), id);
            match envelope::parse_response(&self.send_http(&body).await?)? {
                ResponseOutcome::Success(value) => return Ok(value),
                ResponseOutcome::RpcError { code, .. }
                    if code == WORK_QUEUE_FULL && retries < self.max_retries =>
                {
                    retries += 1;
                    emit::retried(method);
                    tokio::time::sleep(self.retry_delay).await;
                }
                ResponseOutcome::RpcError { code, message } => {
                    return Err(RpcError::Rpc { code, message })
                }
            }
        }
    }

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
        read_body_capped(response, MAX_RESPONSE_BYTES).await
    }
}

/// Chunk-wise (never allocates an oversized body; a lying `Content-Length` is caught by the
/// running total)
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, RpcError> {
    if response.content_length().is_some_and(|len| len > max as u64) {
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
