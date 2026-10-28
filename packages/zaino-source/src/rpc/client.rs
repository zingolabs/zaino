//! JSON-RPC 2.0 over HTTP to one validator: auth, capped body
//!
//! - one attempt per call: which validator, how many in flight, retries = `zaino-traffic`'s
//! - a batch item's refusal (work queue full included) = that item's outcome, never re-sent here

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicI64, Ordering};

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

/// Largest reply buffered: one full block's reply ..= 32 MiB (no OOM from one hostile reply)
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
    pub(crate) fn new(config: RpcClientConfig) -> Self {
        let limits = config.limits;
        let client = reqwest::Client::builder()
            .connect_timeout(config.timeouts.connect)
            // per read (headers + each body chunk), reset on progress
            .read_timeout(config.timeouts.read)
            .redirect(reqwest::redirect::Policy::none())
            // idle connections kept = the most ever open at once
            .pool_max_idle_per_host(
                usize::try_from(limits.max_connections.get()).expect("u32 fits usize"),
            )
            .build()
            .expect("reqwest without TLS features + no fallible builder option = infallible build");
        let wire = Wire::Http { client, url: config.url, auth: config.auth };
        Self::over(config.name, wire, limits)
    }

    /// Every request answered by `serve` in-process (`MockValidator`)
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn served(name: String, serve: Served, limits: LinkLimits) -> Self {
        Self::over(name, Wire::Served(serve), limits)
    }

    fn over(name: String, wire: Wire, limits: LinkLimits) -> Self {
        let id_counter = AtomicI64::new(0);
        Self { name, wire, id_counter, response_size_cap: limits.response_size_cap }
    }

    pub(crate) fn response_size_cap(&self) -> ResponseSizeCap {
        self.response_size_cap
    }

    /// Metric label (the configured address)
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn received(&self, n: usize) {
        metrics::received(&self.name, n);
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcError> {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let body = envelope::build_request(method, &params, id);
        let call = async { envelope::parse_response(&self.send_http(&body).await?) };
        metrics::observed(&self.name, method, call, rpc_failure).await
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
        let calls_in_batch = i64::try_from(calls.len()).expect("a batch holds far under i64::MAX");
        let first = self.id_counter.fetch_add(calls_in_batch, Ordering::Relaxed);
        let body = envelope::build_batch(calls.iter(), first);
        let call =
            async { envelope::parse_batch(&self.send_http(&body).await?, first, calls.len()) };
        metrics::observed(&self.name, label, call, rpc_failure).await
    }

    async fn send_http(&self, body: &Value) -> Result<Vec<u8>, RpcError> {
        let (client, url, auth) = match &self.wire {
            Wire::Http { client, url, auth } => (client, url, auth),
            #[cfg(any(test, feature = "testing"))]
            Wire::Served(serve) => {
                return self.read_served(serve(body.to_string().into_bytes()).await?)
            }
        };
        let mut request =
            client.post(url).header("Content-Type", "application/json").body(body.to_string());
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

    /// In-process reply under the same cap, counted as one chunk
    #[cfg(any(test, feature = "testing"))]
    fn read_served(&self, body: Vec<u8>) -> Result<Vec<u8>, RpcError> {
        let max = self.response_size_cap.bytes();
        if body.len() > max {
            return Err(RpcError::ResponseBodyTooLarge { max });
        }
        self.received(body.len());
        Ok(body)
    }

    /// Chunk-wise (never allocates an oversized body; lying `Content-Length` caught by the total)
    async fn read_body_capped(&self, mut response: reqwest::Response) -> Result<Vec<u8>, RpcError> {
        let max = self.response_size_cap.bytes();
        let too_large = RpcError::ResponseBodyTooLarge { max };
        // past usize = past any cap
        let announced =
            response.content_length().map(|len| usize::try_from(len).unwrap_or(usize::MAX));
        if announced.is_some_and(|len| len > max) {
            return Err(too_large);
        }

        // sized once from `Content-Length` (≤ max): no regrowth copies of a multi-MB body
        let mut body = Vec::with_capacity(announced.unwrap_or(0));
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > max {
                return Err(too_large);
            }
            self.received(chunk.len());
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn rpc_failure<T>(settled: &Result<T, RpcError>) -> Option<crate::FailureMode> {
    settled.as_ref().err().map(RpcError::failure_mode)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;

    use super::*;

    /// Window a re-sent request would arrive in (the client never re-sends: the balancer does)
    const RESEND_WINDOW: Duration = Duration::from_millis(200);

    /// Loopback HTTP validator for one exchange: request body → `answer` → `200` reply
    ///
    /// - task → (request body, whether a second connection came within `RESEND_WINDOW`)
    async fn validator_answering(
        answer: fn(&Value) -> Value,
    ) -> (String, JoinHandle<(Value, bool)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("bound"));
        let exchange = tokio::spawn(async move {
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
                    break serde_json::from_str::<Value>(body).expect("json body");
                }
            };
            let reply = answer(&body).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
            let resent = tokio::time::timeout(RESEND_WINDOW, listener.accept()).await.is_ok();
            (body, resent)
        });
        (url, exchange)
    }

    fn client(name: &str, url: String) -> RpcClient {
        let limits = LinkLimits::default();
        let config = RpcClientConfig {
            url,
            name: name.to_owned(),
            auth: None,
            timeouts: Timeouts::default(),
            limits,
        };
        RpcClient::new(config)
    }

    /// Batch replies reversed by the validator: outcomes still in call order, `-1` busy = that
    /// item's own refusal (never re-sent here: the balancer's retry)
    #[tokio::test]
    async fn a_batch_answers_in_call_order_each_item_its_own_outcome() {
        let (url, validator) = validator_answering(|batch| {
            let reply = |call: &Value| match call["params"][0].as_str() {
                Some("x") => json!({ "id": call["id"], "result": "a" }),
                Some("y") => {
                    json!({ "id": call["id"], "error": { "code": -1, "message": "busy" } })
                }
                _ => json!({ "id": call["id"], "error": { "code": -5, "message": "gone" } }),
            };
            let calls = batch.as_array().expect("a batch");
            Value::Array(calls.iter().rev().map(reply).collect())
        })
        .await;
        let calls = ["x", "y", "z"]
            .map(|param| Call { method: "getrawtransaction", params: vec![Value::from(param)] })
            .to_vec();

        let outcomes: Vec<Result<String, RpcError>> =
            client("validator", url).call_batch("getrawtransaction", calls).await.expect("batch");

        let outcomes: Vec<String> = outcomes
            .into_iter()
            .map(|outcome| match outcome {
                Ok(value) => value,
                Err(RpcError::Rpc { code, .. }) => format!("rpc {code}"),
                Err(other) => format!("{other}"),
            })
            .collect();
        assert_eq!(outcomes, ["a", "rpc -1", "rpc -5"], "outcomes for calls x, y, z");
        let (received, resent) = validator.await.expect("validator task");
        let sent: Vec<&str> = received
            .as_array()
            .expect("a batch")
            .iter()
            .map(|call| call["params"][0].as_str().expect("param"))
            .collect();
        assert_eq!(sent, ["x", "y", "z"], "one request, call order");
        assert!(!resent, "busy item not re-sent within {RESEND_WINDOW:?}");
    }

    /// Latency = round trips the validator answered: an error reply (`-8`) = timed + counted
    /// `rpc_-8`, a refused connection = counted `connection`, never timed
    #[tokio::test]
    async fn only_an_answered_attempt_is_timed_a_refused_one_is_only_counted() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let rendered = recorder.handle();
        let _recording = ::metrics::set_default_local_recorder(&recorder);
        let (answering, _) = validator_answering(
            |call| json!({ "id": call["id"], "error": { "code": -8, "message": "above the tip" } }),
        )
        .await;
        // bound then dropped: nothing listens
        let refusing = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            format!("http://{}", gone.local_addr().expect("bound"))
        };

        let answered =
            client("answering", answering).call::<Value>("getblock", vec!["99".into()]).await;
        let refused =
            client("refusing", refusing).call::<Value>("getblock", vec!["99".into()]).await;

        assert!(matches!(answered, Err(RpcError::Rpc { code: -8, .. })), "answering: {answered:?}");
        assert!(matches!(refused, Err(RpcError::Http(_))), "refusing: {refused:?}");
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
