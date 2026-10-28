//! Admin surface: `/metrics`, `/livez`, `/readyz`, on a runtime of its own
//!
//! - Own thread + current-thread runtime: a probe answered from the saturated serving runtime
//!   measures its queue, and a timed-out liveness probe gets the pod killed
//! - `/readyz` + `/metrics`' gauges = [`DaemonStatus`] (one `GrpcSnapshot` load each)
//! - `/readyz` answered inline

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use http_body_util::Full;
use hyper::{
    body::Bytes, header::HeaderValue, server::conn::http1, service::service_fn, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::DaemonStatus;
use crate::error::IndexerError;
use crate::telemetry::metrics::{install_recorder, scrape, Format};

/// Bounds `AtomicBucket` growth between scrapes (`render` drains itself, upkeep covers no scraper)
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Concurrent admin connections (rest wait in the accept backlog)
///
/// - Unbounded `spawn` per connection = fd exhaustion of the whole process (may bind non-private)
const MAX_CONNECTIONS: usize = 32;

/// Heartbeat age failing `/livez` (wedged serving runtime stops [`beat`], admin thread answers)
const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(30);

/// [`beat`] cadence
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Last beat from the serving runtime
#[derive(Default)]
pub(crate) struct Heartbeat(watch::Sender<Option<Instant>>);

impl Heartbeat {
    pub(crate) fn beat(&self) {
        self.0.send_replace(Some(Instant::now()));
    }

    pub(crate) fn live(&self) -> bool {
        is_fresh(self.0.borrow().map(|at| at.elapsed()))
    }
}

/// Heartbeat from the serving runtime until `cancel` (wedged runtime → `/livez` fails)
pub(crate) async fn beat(status: Arc<DaemonStatus>, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => status.beat(),
            () = cancel.cancelled() => return,
        }
    }
}

/// `None` = never beaten = still starting (k8s covers that window with its own startup delay)
fn is_fresh(age: Option<Duration>) -> bool {
    age.is_none_or(|age| age < HEARTBEAT_MAX_AGE)
}

/// Bound + serving on its own thread (answers however busy the workers)
///
/// - Bind first (recorder before listener = undrained samples), bind failure fails startup
pub(crate) fn start(endpoint: SocketAddr, status: Arc<DaemonStatus>) -> Result<(), IndexerError> {
    let listener = bind(endpoint)?;
    let handle = install_recorder();
    spawn(listener, handle, status)?;
    info!(%endpoint, "Listening");
    Ok(())
}

/// Before the recorder installs (no sample without a draining listener)
fn bind(endpoint: SocketAddr) -> Result<std::net::TcpListener, IndexerError> {
    let listener = std::net::TcpListener::bind(endpoint).map_err(|e| {
        IndexerError::Metrics(format!("admin endpoint {endpoint} failed to bind: {e}"))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| IndexerError::Metrics(format!("admin endpoint {endpoint}: {e}")))?;
    Ok(listener)
}

/// Own thread + runtime
fn spawn(
    listener: std::net::TcpListener,
    handle: PrometheusHandle,
    status: Arc<DaemonStatus>,
) -> Result<(), IndexerError> {
    std::thread::Builder::new()
        .name("zaino-admin".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => return error!(%e, "admin runtime failed to build; no /metrics or probes"),
            };
            runtime.block_on(serve(listener, handle, status));
        })
        .map_err(|e| IndexerError::Metrics(format!("failed to spawn admin thread: {e}")))?;
    Ok(())
}

async fn serve(
    listener: std::net::TcpListener,
    handle: PrometheusHandle,
    status: Arc<DaemonStatus>,
) {
    // Bound before the recorder installed ([`start`]): this loop or a scrape drains all
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(UPKEEP_INTERVAL);
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });

    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(e) => return error!(%e, "admin listener failed to register with its runtime"),
    };

    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        // Held for the connection's life (the cap bounds fds and tasks)
        let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Backoff, not `continue`: EMFILE keeps the listener readable → 100% CPU spin
                warn!(%e, "admin connection failed to accept");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (handle, status) = (handle.clone(), Arc::clone(&status));
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                let (handle, status) = (handle.clone(), Arc::clone(&status));
                async move { Ok::<_, Infallible>(route(&handle, status, &request).await) }
            });
            // `timer` load-bearing: without it hyper drops its header-read timeout (slowloris)
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn route(
    handle: &PrometheusHandle,
    status: Arc<DaemonStatus>,
    request: &Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    match request.uri().path() {
        // Off the admin runtime: `render` walks every handle, and this thread answers probes
        "/metrics" => {
            let handle = handle.clone();
            let format = negotiate(request.headers().get(hyper::header::ACCEPT));
            let scrape = move || scrape(&handle, &status, format);
            body(StatusCode::OK, format.content_type(), zaino_sync::blocking(scrape).await)
        }
        "/livez" => match status.live() {
            true => body(StatusCode::OK, PLAIN_CONTENT_TYPE, "ok".to_string()),
            false => {
                body(StatusCode::SERVICE_UNAVAILABLE, PLAIN_CONTENT_TYPE, "unavailable".to_string())
            }
        },
        "/readyz" => {
            let (ready, json) = status.readiness_json();
            let code = if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
            body(code, JSON_CONTENT_TYPE, json)
        }
        _ => body(StatusCode::NOT_FOUND, PLAIN_CONTENT_TYPE, String::new()),
    }
}

/// Protobuf when any `Accept` entry offers it with q > 0 (Prometheus / Alloy: listed first, weighted)
fn negotiate(accept: Option<&HeaderValue>) -> Format {
    let offers_protobuf = |entry: &str| {
        let mut parts = entry.split(';').map(str::trim);
        let refused =
            |param: &str| param.strip_prefix("q=").and_then(|q| q.parse::<f64>().ok()) == Some(0.0);
        parts.next() == Some("application/vnd.google.protobuf") && !parts.any(refused)
    };
    match accept.and_then(|accept| accept.to_str().ok()) {
        Some(accept) if accept.split(',').any(offers_protobuf) => Format::Protobuf,
        _ => Format::Text,
    }
}

const PLAIN_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

const JSON_CONTENT_TYPE: &str = "application/json";

fn body(
    status: StatusCode,
    content_type: &'static str,
    payload: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .body(Full::new(payload.into()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end over a real socket: bind, accept loop, hyper wiring, routing, scrape negotiation
    #[tokio::test]
    async fn the_admin_surface_answers_metrics_and_probes() {
        let (recorder, handle) = crate::telemetry::metrics::recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("zaino.test.total").increment(7);
            metrics::histogram!("zaino.test.seconds").record(0.042);
        });

        let listener = bind("127.0.0.1:0".parse().expect("a loopback socket address parses"))
            .expect("loopback bind succeeds");
        let endpoint = listener.local_addr().expect("a bound listener has an address");
        let network = zcash_protocol::consensus::NetworkType::Regtest;
        tokio::spawn(serve(listener, handle, Arc::new(DaemonStatus::new(network, []))));

        // listener bound before the spawn: a connect lands in its backlog, no retry
        let client = reqwest::Client::new();
        let get = |path: &'static str, accept: &'static str| {
            let request = client.get(format!("http://{endpoint}{path}"));
            async move {
                let response = request.header("accept", accept).send().await;
                let response = response.unwrap_or_else(|e| panic!("GET {path}: {e}"));
                let content_type = response.headers().get("content-type").cloned();
                let content_type =
                    content_type.map(|v| v.to_str().unwrap_or("non-ASCII").to_owned());
                let status = response.status().as_u16();
                // lossy: protobuf bodies are binary
                let body =
                    response.bytes().await.unwrap_or_else(|e| panic!("GET {path} body: {e}"));
                (status, content_type, String::from_utf8_lossy(&body).into_owned())
            }
        };

        // Prometheus 3 / Alloy with native histograms on: protobuf first, text as fallback
        let (status, content_type, protobuf) = get(
            "/metrics",
            "application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;\
             encoding=delimited;q=0.6,text/plain;version=0.0.4;q=0.4,*/*;q=0.1",
        )
        .await;
        assert_eq!(status, 200, "protobuf scrape: {protobuf}");
        let protobuf_type = content_type.as_deref().unwrap_or_default();
        assert!(protobuf_type.starts_with("application/vnd.google.protobuf"), "{content_type:?}");
        for family in ["zaino_test_total", "zaino_test_seconds"] {
            assert!(protobuf.contains(family), "{family} missing from the protobuf scrape");
        }

        // Text (curl, or protobuf refused): counters only, a native histogram has no text form
        for accept in ["*/*", "application/vnd.google.protobuf;q=0,text/plain"] {
            let (status, content_type, text) = get("/metrics", accept).await;
            assert_eq!(status, 200, "{accept}: {text}");
            let text_type = content_type.as_deref().unwrap_or_default();
            assert!(
                text_type.starts_with("text/plain; version=0.0.4"),
                "{accept}: {content_type:?}"
            );
            assert!(text.contains("zaino_test_total 7"), "{accept}: {text}");
            assert!(!text.contains("zaino_test_seconds"), "{accept}: histogram not native: {text}");
        }

        let (livez, _, body) = get("/livez", "*/*").await;
        assert_eq!(livez, 200, "/livez before any beat (still starting): {body}");
        // no indexer booted in this test = sources unpublished
        let readyz = get("/readyz", "*/*").await;
        let provisioning = r#"{"state":"provisioning","detail":null}"#.to_owned();
        assert_eq!((readyz.0, readyz.2), (503, provisioning), "/readyz (status, body) before boot");
        let (unknown, _, body) = get("/nope", "*/*").await;
        assert_eq!(unknown, 404, "/nope: {body}");
    }

    #[test]
    fn a_stale_heartbeat_is_not_live() {
        let under =
            HEARTBEAT_MAX_AGE.checked_sub(Duration::from_millis(1)).expect("max age > 1 ms");
        for (case, age, fresh) in [
            ("just beaten", Some(Duration::ZERO), true),
            ("a tick under the max age", Some(under), true),
            ("stale = wedged runtime", Some(HEARTBEAT_MAX_AGE), false),
            ("never beaten = still starting", None, true),
        ] {
            assert_eq!(is_fresh(age), fresh, "{case}");
        }
    }
}
