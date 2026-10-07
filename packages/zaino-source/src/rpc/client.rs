//! JSON-RPC 2.0 over HTTP to one validator: auth, capped body, work-queue-full retry, and the
//! budget every consumer of that validator shares
//!
//! ```text
//!               ┌─ Control  2 permits   (tip, listing, headers, broadcast)
//!   RpcClient ──┼─ Serve    ¼           (GetTransaction, address-transaction bytes)  ─▶ validator
//!               └─ Sync     the rest    (bulk block fetch)
//!                  + requests/s, bytes/s (optional, GCRA)
//! ```
//!
//! - permits = connections (HTTP/1.1 = one request per connection): Σ = `max_connections`
//! - lanes never borrow: a sync burst or a wallet storm cannot delay the tip poll

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Semaphore;

use super::emit;
use super::envelope;
use super::error::RpcError;

/// Largest body buffered (no OOM from one hostile reply; a full block = far below)
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Zebra's "work queue full": busy, not broken
const WORK_QUEUE_FULL: i64 = -1;

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

/// What a request is for (its own permit pool)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Control,
    Serve,
    Sync,
}

impl Lane {
    const ALL: [Lane; 3] = [Lane::Control, Lane::Serve, Lane::Sync];

    /// Metric label
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Lane::Control => "control",
            Lane::Serve => "serve",
            Lane::Sync => "sync",
        }
    }
}

/// What Zaino may put on one validator; `None` = unlimited
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLimits {
    max_connections: NonZeroU32,
    pub max_requests_per_sec: Option<NonZeroU32>,
    pub max_bytes_per_sec: Option<NonZeroU32>,
}

impl LinkLimits {
    /// Lane split needs 4 (control 2, serve ≥ 1, sync ≥ 1)
    pub const MIN_CONNECTIONS: u32 = 4;

    /// `None` below [`MIN_CONNECTIONS`](Self::MIN_CONNECTIONS)
    pub fn new(
        max_connections: NonZeroU32,
        max_requests_per_sec: Option<NonZeroU32>,
        max_bytes_per_sec: Option<NonZeroU32>,
    ) -> Option<Self> {
        (max_connections.get() >= Self::MIN_CONNECTIONS).then_some(Self {
            max_connections,
            max_requests_per_sec,
            max_bytes_per_sec,
        })
    }

    pub fn max_connections(&self) -> NonZeroU32 {
        self.max_connections
    }

    fn permits(&self, lane: Lane) -> usize {
        let total = self.max_connections.get() as usize;
        let serve = (total / 4).max(1);
        match lane {
            Lane::Control => 2,
            Lane::Serve => serve,
            Lane::Sync => total - 2 - serve,
        }
    }
}

impl Default for LinkLimits {
    fn default() -> Self {
        let connections = NonZeroU32::new(32).expect("32 is non-zero");
        Self::new(connections, None, None).expect("32 ≥ MIN_CONNECTIONS")
    }
}

/// - `name` = metric label (the validator's configured address); `auth` = basic (user, password)
/// - `max_retries` = re-sends after a work-queue-full refusal
pub struct RpcClientConfig {
    pub url: String,
    pub name: String,
    pub auth: Option<(String, String)>,
    pub timeouts: Timeouts,
    pub limits: LinkLimits,
    pub max_retries: u32,
    pub retry_delay: Duration,
}

impl Default for RpcClientConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8232".to_string(),
            name: "127.0.0.1:8232".to_string(),
            auth: None,
            timeouts: Timeouts::default(),
            limits: LinkLimits::default(),
            max_retries: 5,
            retry_delay: Duration::from_millis(500),
        }
    }
}

/// `lanes` indexed `lane as usize` (`Lane::ALL` order)
pub struct RpcClient {
    url: String,
    name: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
    id_counter: AtomicI64,
    max_retries: u32,
    retry_delay: Duration,
    lanes: [Semaphore; 3],
    requests: Option<DefaultDirectRateLimiter>,
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
            .pool_max_idle_per_host(limits.max_connections().get() as usize)
            .build()?;
        let per_second = |rate: NonZeroU32| RateLimiter::direct(Quota::per_second(rate));

        Ok(Self {
            url: config.url,
            name: config.name,
            client,
            auth: config.auth,
            id_counter: AtomicI64::new(0),
            max_retries: config.max_retries,
            retry_delay: config.retry_delay,
            lanes: Lane::ALL.map(|lane| Semaphore::new(limits.permits(lane))),
            requests: limits.max_requests_per_sec.map(per_second),
            bytes: limits.max_bytes_per_sec.map(per_second),
        })
    }

    /// `result` as a raw `Value`
    pub async fn call(
        &self,
        lane: Lane,
        method: &str,
        params: Vec<Value>,
    ) -> Result<Value, RpcError> {
        self.call_as(lane, method, params).await
    }

    /// `result` deserialized straight off the body as `T` (no `Value` hop: a multi-MB hex block
    /// is decoded from the body bytes once)
    ///
    /// - waits for a `lane` permit, then the request budget; the permit is held to the last byte
    pub(crate) async fn call_as<T: DeserializeOwned>(
        &self,
        lane: Lane,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let queued = Instant::now();
        let _permit =
            self.lanes[lane as usize].acquire().await.expect("lane semaphore never closed");
        if let Some(requests) = &self.requests {
            requests.until_ready().await;
        }
        emit::waited(&self.name, lane, queued.elapsed());
        let _in_flight = emit::InFlight::start(&self.name, lane);

        let started = Instant::now();
        let settled = self.send_until_settled(method, params).await;
        emit::call_settled(&self.name, method, started.elapsed(), settled.as_ref().err());
        settled
    }

    /// `calls` in one HTTP request, one outcome per item (call order); `Err` = whole batch failed
    ///
    /// - one `lane` permit; request budget charged per item (validator work = items, not POSTs)
    /// - work-queue-full items re-sent as a smaller batch, settled items kept
    /// - metric `method` = the calls' one method, or `batch` when mixed
    pub(crate) async fn call_batch<T: DeserializeOwned>(
        &self,
        lane: Lane,
        calls: Vec<Call>,
    ) -> Result<Vec<Result<T, RpcError>>, RpcError> {
        let queued = Instant::now();
        let _permit =
            self.lanes[lane as usize].acquire().await.expect("lane semaphore never closed");
        if let Some(requests) = &self.requests {
            charge(requests, calls.len()).await;
        }
        emit::waited(&self.name, lane, queued.elapsed());
        let _in_flight = emit::InFlight::start(&self.name, lane);

        let label = match calls.split_first() {
            Some((first, rest)) if rest.iter().all(|call| call.method == first.method) => {
                first.method
            }
            _ => "batch",
        };
        let started = Instant::now();
        let settled = self.send_batch_until_settled(label, &calls).await;
        emit::call_settled(&self.name, label, started.elapsed(), settled.as_ref().err());
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
            let body = envelope::build_request(method, &params, id);
            match envelope::parse_response(&self.send_http(&body).await?) {
                Err(RpcError::Rpc { code: WORK_QUEUE_FULL, .. }) if retries < self.max_retries => {
                    retries += 1;
                    emit::retried(&self.name, method);
                    tokio::time::sleep(self.retry_delay).await;
                }
                settled => return settled,
            }
        }
    }

    async fn send_batch_until_settled<T: DeserializeOwned>(
        &self,
        label: &str,
        calls: &[Call],
    ) -> Result<Vec<Result<T, RpcError>>, RpcError> {
        let mut settled: Vec<Option<Result<T, RpcError>>> =
            (0..calls.len()).map(|_| None).collect();
        let mut pending: Vec<usize> = (0..calls.len()).collect();
        let mut retries = 0u32;
        while !pending.is_empty() {
            let first = self.id_counter.fetch_add(pending.len() as i64, Ordering::Relaxed);
            let body = envelope::build_batch(pending.iter().map(|&index| &calls[index]), first);
            let replies =
                envelope::parse_batch(&self.send_http(&body).await?, first, pending.len())?;

            let mut busy = Vec::new();
            for (index, reply) in pending.into_iter().zip(replies) {
                match reply {
                    Err(RpcError::Rpc { code: WORK_QUEUE_FULL, .. })
                        if retries < self.max_retries =>
                    {
                        busy.push(index)
                    }
                    reply => settled[index] = Some(reply),
                }
            }
            pending = busy;
            if !pending.is_empty() {
                retries += 1;
                emit::retried(&self.name, label);
                tokio::time::sleep(self.retry_delay).await;
            }
        }
        Ok(settled
            .into_iter()
            .map(|item| item.expect("loop ends once every item settled"))
            .collect())
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

    /// Control 2 whatever the total; serve ¼ (>= 1); sync the rest; total < 4 = no split
    #[test]
    fn lanes_split_the_connection_cap_and_never_share_it() {
        let split = |total: u32| {
            let limits = LinkLimits::new(NonZeroU32::new(total).expect("nz"), None, None)?;
            Some(Lane::ALL.map(|lane| limits.permits(lane)))
        };
        assert_eq!(split(3), None);
        assert_eq!(split(4), Some([2, 1, 1]));
        assert_eq!(split(16), Some([2, 4, 10]));
        assert_eq!(split(32), Some([2, 8, 22]));
    }

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
    /// - work-queue-full item re-sent alone; settled refusal kept (never re-sent)
    /// - every outcome in call order
    #[tokio::test]
    async fn a_batch_resends_only_its_busy_items_and_answers_in_call_order() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("bound"));
        // "x" → "a"; "y" → busy once, then "b"; "z" → not found
        let validator = tokio::spawn(async move {
            let mut received: Vec<Value> = Vec::new();
            let mut busy_once = true;
            while received.len() < 2 {
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
                        "y" if busy_once => {
                            busy_once = false;
                            serde_json::json!({ "id": call["id"], "error": { "code": -1, "message": "busy" } })
                        }
                        "y" => serde_json::json!({ "id": call["id"], "result": "b" }),
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
                received.push(body);
            }
            received
        });

        let client = RpcClient::new(RpcClientConfig {
            url,
            retry_delay: Duration::ZERO,
            ..RpcClientConfig::default()
        })
        .expect("client");
        let calls = ["x", "y", "z"]
            .map(|param| Call { method: "getrawtransaction", params: vec![Value::from(param)] })
            .to_vec();
        let outcomes: Vec<Result<String, RpcError>> =
            client.call_batch(Lane::Control, calls).await.expect("batch");
        let outcomes: Vec<String> = outcomes
            .into_iter()
            .map(|outcome| match outcome {
                Ok(value) => value,
                Err(RpcError::Rpc { code, .. }) => format!("rpc {code}"),
                Err(other) => format!("{other}"),
            })
            .collect();
        assert_eq!(outcomes, ["a", "b", "rpc -5"]);

        let received = validator.await.expect("validator task");
        let sent = |batch: &Value| -> Vec<String> {
            let calls = batch.as_array().expect("a batch").iter();
            calls.map(|call| call["params"][0].as_str().expect("param").to_owned()).collect()
        };
        assert_eq!(sent(&received[0]), ["x", "y", "z"]);
        assert_eq!(sent(&received[1]), ["y"], "only the busy item re-sent");
    }
}
