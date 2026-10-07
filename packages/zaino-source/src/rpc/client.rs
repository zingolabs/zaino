//! JSON-RPC 2.0 over HTTP to one validator: auth, capped body, response bytes paced as read
//!
//! - one attempt per call: which validator, how many in flight, retries = `zaino-traffic`'s
//! - a batch item's refusal (work queue full included) = that item's outcome, never re-sent here

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::emit;
use super::envelope;
use super::error::RpcError;

/// Largest body buffered (no OOM from one hostile reply; a full block = far below)
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// - `read` = silence, never total duration (a multi-MB block over a slow link must finish; a
///   total deadline below its transfer time = retry livelock)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { connect: Duration::from_secs(2), read: Duration::from_secs(30) }
    }
}

/// One JSON-RPC call (a batch item)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: &'static str,
    pub(crate) params: Vec<Value>,
}

/// One validator's link: idle connections kept, response bytes per second (`None` = unlimited)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLimits {
    pub max_connections: NonZeroU32,
    pub max_bytes_per_sec: Option<NonZeroU32>,
}

impl Default for LinkLimits {
    fn default() -> Self {
        let max_connections = NonZeroU32::new(32).expect("32 is non-zero");
        Self { max_connections, max_bytes_per_sec: None }
    }
}

/// `name` = metric label (the validator's configured address); `auth` = basic (user, password)
pub struct RpcClientConfig {
    pub url: String,
    pub name: String,
    pub auth: Option<(String, String)>,
    pub timeouts: Timeouts,
    pub limits: LinkLimits,
}

impl Default for RpcClientConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8232".to_string(),
            name: "127.0.0.1:8232".to_string(),
            auth: None,
            timeouts: Timeouts::default(),
            limits: LinkLimits::default(),
        }
    }
}

pub struct RpcClient {
    url: String,
    name: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
    id_counter: AtomicI64,
    bytes: Option<DefaultDirectRateLimiter>,
}

impl RpcClient {
    pub fn new(config: RpcClientConfig) -> Result<Self, RpcError> {
        let limits = config.limits;
        let client = reqwest::Client::builder()
            .connect_timeout(config.timeouts.connect)
            // per read (headers + each body chunk), reset on progress
            .read_timeout(config.timeouts.read)
            .redirect(reqwest::redirect::Policy::none())
            // idle connections kept = the most ever open at once
            .pool_max_idle_per_host(limits.max_connections.get() as usize)
            .build()?;
        let per_second = |rate: NonZeroU32| RateLimiter::direct(Quota::per_second(rate));

        Ok(Self {
            url: config.url,
            name: config.name,
            client,
            auth: config.auth,
            id_counter: AtomicI64::new(0),
            bytes: limits.max_bytes_per_sec.map(per_second),
        })
    }

    /// `result` as a raw `Value`
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, RpcError> {
        self.call_as(method, params).await
    }

    /// `result` deserialized straight off the body as `T` (no `Value` hop: a multi-MB hex block
    /// is decoded from the body bytes once)
    pub(crate) async fn call_as<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let _in_flight = emit::InFlight::start(&self.name);
        let started = Instant::now();
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let body = envelope::build_request(method, &params, id);
        let settled =
            self.send_http(&body).await.and_then(|reply| envelope::parse_response(&reply));
        emit::call_settled(&self.name, method, started.elapsed(), settled.as_ref().err());
        settled
    }

    /// `calls` in one HTTP request, one outcome per item (call order); `Err` = whole batch failed
    ///
    /// - metric `method` = the calls' one method, or `batch` when mixed
    pub(crate) async fn call_batch<T: DeserializeOwned>(
        &self,
        calls: Vec<Call>,
    ) -> Result<Vec<Result<T, RpcError>>, RpcError> {
        let _in_flight = emit::InFlight::start(&self.name);
        let label = match calls.split_first() {
            Some((first, rest)) if rest.iter().all(|call| call.method == first.method) => {
                first.method
            }
            _ => "batch",
        };
        let started = Instant::now();
        let first = self.id_counter.fetch_add(calls.len() as i64, Ordering::Relaxed);
        let body = envelope::build_batch(calls.iter(), first);
        let settled = self.send_http(&body).await;
        let settled = settled.and_then(|reply| envelope::parse_batch(&reply, first, calls.len()));
        emit::call_settled(&self.name, label, started.elapsed(), settled.as_ref().err());
        settled
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
        let body = read_body_capped(response, MAX_RESPONSE_BYTES, self.bytes.as_ref()).await?;
        emit::received(&self.name, body.len());
        Ok(body)
    }
}

/// Chunk-wise (never allocates an oversized body; a lying `Content-Length` is caught by the
/// running total)
///
/// - `bytes` charged per chunk as read: exhausted budget = reading stops, TCP backpressure slows
///   the sender (pacing, not after-the-fact accounting)
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
    bytes: Option<&DefaultDirectRateLimiter>,
) -> Result<Vec<u8>, RpcError> {
    if response.content_length().is_some_and(|len| len > max as u64) {
        return Err(RpcError::ResponseBodyTooLarge { max });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(RpcError::ResponseBodyTooLarge { max });
        }
        if let Some(bytes) = bytes {
            charge(bytes, chunk.len()).await;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `n` cells, in pieces no larger than the limiter's burst (one second's worth)
async fn charge(limiter: &DefaultDirectRateLimiter, n: usize) {
    let mut left = n;
    while let Some(piece) = NonZeroU32::new(u32::try_from(left).unwrap_or(u32::MAX)) {
        match limiter.until_n_ready(piece).await {
            Ok(()) => return,
            Err(governor::InsufficientCapacity(burst)) => {
                let burst = NonZeroU32::new(burst).expect("a quota's burst is non-zero");
                limiter.until_n_ready(burst).await.expect("the burst always fits");
                left -= burst.get() as usize;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chunk > one-second burst → charged in burst-sized pieces (else `InsufficientCapacity`,
    /// never read)
    #[tokio::test]
    async fn a_chunk_larger_than_the_burst_is_paced_not_refused() {
        let limiter = RateLimiter::direct(Quota::per_second(NonZeroU32::new(1_000).expect("nz")));
        let started = Instant::now();
        charge(&limiter, 2_500).await;
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(1_400),
            "2.5 s of budget after a 1 s burst: {took:?}"
        );
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    /// Real HTTP, validator answering out of order
    ///
    /// - one request: every outcome in call order, a work-queue-full item its own refusal (never
    ///   re-sent here: the balancer's retry)
    #[tokio::test]
    async fn a_batch_answers_in_call_order_each_item_its_own_outcome() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("bound"));
        // "x" → "a"; "y" → busy; "z" → not found
        let validator = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let body = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.expect("read");
                request.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&request);
                let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase().strip_prefix("content-length: ")?.parse().ok()
                    })
                    .expect("content-length");
                if body.len() == length {
                    break serde_json::from_str::<Value>(body).expect("json batch");
                }
            };
            let mut replies: Vec<Value> = body
                .as_array()
                .expect("a batch")
                .iter()
                .map(|call| match call["params"][0].as_str().expect("param") {
                    "x" => serde_json::json!({ "id": call["id"], "result": "a" }),
                    "y" => serde_json::json!({ "id": call["id"], "error": { "code": -1, "message": "busy" } }),
                    _ => serde_json::json!({ "id": call["id"], "error": { "code": -5, "message": "gone" } }),
                })
                .collect();
            replies.reverse();
            let reply = serde_json::to_string(&replies).expect("reply");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
            let next = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
            (body, next.is_ok())
        });

        let client =
            RpcClient::new(RpcClientConfig { url, ..RpcClientConfig::default() }).expect("client");
        let calls = ["x", "y", "z"]
            .map(|param| Call { method: "getrawtransaction", params: vec![Value::from(param)] })
            .to_vec();
        let outcomes: Vec<Result<String, RpcError>> =
            client.call_batch(calls).await.expect("batch");
        let outcomes: Vec<String> = outcomes
            .into_iter()
            .map(|outcome| match outcome {
                Ok(value) => value,
                Err(RpcError::Rpc { code, .. }) => format!("rpc {code}"),
                Err(other) => format!("{other}"),
            })
            .collect();
        assert_eq!(outcomes, ["a", "rpc -1", "rpc -5"]);

        let (received, resent) = validator.await.expect("validator task");
        let sent: Vec<&str> = received
            .as_array()
            .expect("a batch")
            .iter()
            .map(|call| call["params"][0].as_str().expect("param"))
            .collect();
        assert_eq!((sent, resent), (vec!["x", "y", "z"], false), "one request, nothing re-sent");
    }
}
