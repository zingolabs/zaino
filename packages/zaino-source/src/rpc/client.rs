//! JSON-RPC 2.0 over HTTP to one validator: auth, capped body, response bytes paced as read
//!
//! - one attempt per call: which validator, how many in flight, retries = `zaino-traffic`'s
//! - a batch item's refusal (work queue full included) = that item's outcome, never re-sent here

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::de::DeserializeOwned;
use serde_json::Value;
use zaino_primitives::protocol::MAX_BLOCK_BYTES;

use super::envelope;
use super::error::RpcError;
use crate::metrics;
use crate::{LinkLimits, Timeouts};

/// JSON-RPC framing around one result (id, version, keys)
pub(crate) const REPLY_ENVELOPE_BYTES: usize = 1024;
/// One `getblock` reply: a full block as hex + its envelope
const BLOCK_REPLY_BYTES: usize = 2 * MAX_BLOCK_BYTES + REPLY_ENVELOPE_BYTES;

/// Largest JSON-RPC reply buffered (no OOM from one hostile reply): one full block's reply ..= 32
/// MiB
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseSizeCap {
    bytes: usize,
    blocks_per_batch: NonZeroUsize,
}

impl ResponseSizeCap {
    pub const MAX: Self = Self {
        bytes: 32 << 20,
        blocks_per_batch: NonZeroUsize::new((32 << 20) / BLOCK_REPLY_BYTES)
            .expect("32 MiB holds a full block's reply"),
    };

    /// `None` below one full block's reply or above [`MAX`](Self::MAX)
    pub fn new(bytes: usize) -> Option<Self> {
        let blocks_per_batch = NonZeroUsize::new(bytes / BLOCK_REPLY_BYTES)?;
        (bytes <= Self::MAX.bytes).then_some(Self { bytes, blocks_per_batch })
    }

    pub fn bytes(self) -> usize {
        self.bytes
    }

    /// Full blocks one `getblock` batch reply holds
    pub const fn blocks_per_batch(self) -> NonZeroUsize {
        self.blocks_per_batch
    }
}

/// One JSON-RPC call (a batch item)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: &'static str,
    pub(crate) params: Vec<Value>,
}

/// - `name` = metric label (the validator's configured address)
/// - `auth` = basic (user, password)
pub(crate) struct RpcClientConfig {
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) auth: Option<(String, String)>,
    pub(crate) timeouts: Timeouts,
    pub(crate) limits: LinkLimits,
}

pub(crate) struct RpcClient {
    name: String,
    wire: Wire,
    id_counter: AtomicI64,
    bytes: Option<DefaultDirectRateLimiter>,
    response_size_cap: ResponseSizeCap,
}

/// Where a request body goes
enum Wire {
    Http {
        client: reqwest::Client,
        url: String,
        auth: Option<(String, String)>,
    },
    /// In-process server: the same bytes, no socket (paused-clock tests)
    #[cfg(any(test, feature = "testing"))]
    Served(Served),
}

/// Request body → response body, as an in-process JSON-RPC server answers it
#[cfg(any(test, feature = "testing"))]
pub(crate) type Served = std::sync::Arc<
    dyn Fn(Vec<u8>) -> futures::future::BoxFuture<'static, Result<Vec<u8>, RpcError>> + Send + Sync,
>;

impl RpcClient {
    pub(crate) fn new(config: RpcClientConfig) -> Result<Self, RpcError> {
        let limits = config.limits;
        let client = reqwest::Client::builder()
            .connect_timeout(config.timeouts.connect)
            // per read (headers + each body chunk), reset on progress
            .read_timeout(config.timeouts.read)
            .redirect(reqwest::redirect::Policy::none())
            // idle connections kept = the most ever open at once
            .pool_max_idle_per_host(limits.max_connections.get() as usize)
            .build()?;
        let wire = Wire::Http { client, url: config.url, auth: config.auth };
        Ok(Self::over(config.name, wire, limits))
    }

    /// Every request answered by `serve` in-process (`MockValidator`)
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn served(name: String, serve: Served, limits: LinkLimits) -> Self {
        Self::over(name, Wire::Served(serve), limits)
    }

    fn over(name: String, wire: Wire, limits: LinkLimits) -> Self {
        let per_second = |rate: NonZeroU32| RateLimiter::direct(Quota::per_second(rate));
        Self {
            name,
            wire,
            id_counter: AtomicI64::new(0),
            bytes: limits.max_bytes_per_sec.map(per_second),
            response_size_cap: limits.response_size_cap,
        }
    }

    pub(crate) fn response_size_cap(&self) -> ResponseSizeCap {
        self.response_size_cap
    }

    /// Metric label (the configured address)
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// `n` bytes received: counted, then paced against `max_bytes_per_sec` (unset = at once)
    ///
    /// - waiting = reading stops, the transport's flow control slows the sender
    pub(crate) async fn received(&self, n: usize) {
        metrics::received(&self.name, n);
        if let Some(bytes) = &self.bytes {
            charge(bytes, n).await;
        }
    }

    /// `result` deserialized straight off the body as `T` (no `Value` hop: a multi-MB hex block
    /// is decoded from the body bytes once)
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let _in_flight = metrics::InFlight::start(&self.name);
        let started = Instant::now();
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let body = envelope::build_request(method, &params, id);
        let settled =
            self.send_http(&body).await.and_then(|reply| envelope::parse_response(&reply));
        let failure = settled.as_ref().err().map(RpcError::failure_mode);
        metrics::call_settled(&self.name, method, started.elapsed(), failure);
        settled
    }

    /// `calls` in one HTTP request, one outcome per item (call order)
    ///
    /// - `Err` = whole batch failed
    /// - `label` = metric `method` (the request's purpose: a mixed batch has no one method)
    pub(crate) async fn call_batch<T: DeserializeOwned>(
        &self,
        label: &'static str,
        calls: Vec<Call>,
    ) -> Result<Vec<Result<T, RpcError>>, RpcError> {
        let _in_flight = metrics::InFlight::start(&self.name);
        let started = Instant::now();
        let first = self.id_counter.fetch_add(calls.len() as i64, Ordering::Relaxed);
        let body = envelope::build_batch(calls.iter(), first);
        let settled = self.send_http(&body).await;
        let settled = settled.and_then(|reply| envelope::parse_batch(&reply, first, calls.len()));
        let failure = settled.as_ref().err().map(RpcError::failure_mode);
        metrics::call_settled(&self.name, label, started.elapsed(), failure);
        settled
    }

    async fn send_http(&self, body: &Value) -> Result<Vec<u8>, RpcError> {
        let (client, url, auth) = match &self.wire {
            Wire::Http { client, url, auth } => (client, url, auth),
            #[cfg(any(test, feature = "testing"))]
            Wire::Served(serve) => {
                return self.read_served(serve(serde_json::to_vec(body)?).await?).await
            }
        };
        let mut request = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);
        if let Some((user, pass)) = auth {
            request = request.basic_auth(user, Some(pass));
        }

        let response = request.send().await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(RpcError::Status(status));
        }
        self.read_body_capped(response).await
    }

    /// An in-process reply under the same cap, counted and paced as one chunk
    #[cfg(any(test, feature = "testing"))]
    async fn read_served(&self, body: Vec<u8>) -> Result<Vec<u8>, RpcError> {
        let max = self.response_size_cap.bytes();
        if body.len() > max {
            return Err(RpcError::ResponseBodyTooLarge { max });
        }
        self.received(body.len()).await;
        Ok(body)
    }

    /// Chunk-wise (never allocates an oversized body)
    ///
    /// - Lying `Content-Length` caught by the running total
    /// - each chunk [`received`](Self::received) as read (pacing, not after-the-fact accounting)
    async fn read_body_capped(&self, mut response: reqwest::Response) -> Result<Vec<u8>, RpcError> {
        let max = self.response_size_cap.bytes();
        let too_large = RpcError::ResponseBodyTooLarge { max };
        let announced = response.content_length();
        if announced.is_some_and(|len| len > max as u64) {
            return Err(too_large);
        }

        // sized once from `Content-Length` (≤ max): no regrowth copies of a multi-MB body
        let mut body = Vec::with_capacity(announced.map_or(0, |len| len as usize));
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > max {
                return Err(too_large);
            }
            self.received(chunk.len()).await;
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
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
    use std::time::Duration;

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
        // "x" → "a", "y" → busy, "z" → not found
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

        let config = RpcClientConfig {
            url,
            name: "validator".to_owned(),
            auth: None,
            timeouts: Timeouts::default(),
            limits: LinkLimits::default(),
        };
        let client = RpcClient::new(config).expect("client");
        let calls = ["x", "y", "z"]
            .map(|param| Call { method: "getrawtransaction", params: vec![Value::from(param)] })
            .to_vec();
        let outcomes: Vec<Result<String, RpcError>> =
            client.call_batch("getrawtransaction", calls).await.expect("batch");
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

    /// Latency = round trips the validator answered:
    ///
    /// - an error reply (`-8`) = answered: timed + counted `rpc_-8`
    /// - a refused connection = no answer: counted `connection`, never timed
    #[tokio::test]
    async fn only_an_answered_attempt_is_timed_a_refused_one_is_only_counted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let rendered = recorder.handle();
        let _recording = ::metrics::set_default_local_recorder(&recorder);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let answering = format!("http://{}", listener.local_addr().expect("bound"));
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.expect("read");
            let reply = r#"{"id":0,"error":{"code":-8,"message":"above the tip"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        });
        // bound then dropped: nothing listens
        let refusing = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            format!("http://{}", gone.local_addr().expect("bound"))
        };

        for (name, url) in [("answering", answering), ("refusing", refusing)] {
            let config = RpcClientConfig {
                url,
                name: name.to_owned(),
                auth: None,
                timeouts: Timeouts::default(),
                limits: LinkLimits::default(),
            };
            let client = RpcClient::new(config).expect("client");
            let settled = client.call::<Value>("getblock", vec![Value::from("99")]).await;
            assert!(settled.is_err(), "{name}: {settled:?}");
        }

        let scrape = rendered.render();
        let missing: Vec<&str> = [
            r#"zaino_validator_rpc_duration_seconds_count{validator="answering",method="getblock"} 1"#,
            r#"zaino_validator_rpc_failures_total{validator="answering",method="getblock",failure="rpc_-8"} 1"#,
            r#"zaino_validator_rpc_failures_total{validator="refusing",method="getblock",failure="connection"} 1"#,
        ]
        .into_iter()
        .filter(|line| !scrape.lines().any(|scraped| scraped == *line))
        .collect();
        assert!(missing.is_empty(), "missing {missing:?} from:\n{scrape}");
        let refused_timed = r#"zaino_validator_rpc_duration_seconds_count{validator="refusing""#;
        assert!(!scrape.contains(refused_timed), "a refused attempt timed:\n{scrape}");
    }
}
