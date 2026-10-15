//! Admin surface: `/metrics`, `/livez`, `/readyz`, on a runtime of its own
//!
//! - Own thread + current-thread runtime: a probe answered from the saturated serving runtime
//!   measures its queue, and a timed-out liveness probe gets the pod killed
//! - `/readyz` + `/metrics`' gauges = [`DaemonStatus`] (one snapshot load each; `/readyz` inline)

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::error::IndexerError;
use crate::status::DaemonStatus;

/// Bounds `AtomicBucket` growth between scrapes (`render` drains itself; this covers no scraper)
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Concurrent admin connections; the rest wait in the accept backlog
///
/// - Unbounded `spawn` per connection = fd exhaustion of the whole process (may bind non-private)
const MAX_CONNECTIONS: usize = 32;

/// Heartbeat age failing `/livez` (wedged serving runtime stops [`beat`]; this thread answers)
const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(30);

/// [`beat`] cadence
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Last beat from the serving runtime (poison-tolerant: the lock only guards a `Copy` swap)
#[derive(Default)]
pub(crate) struct Heartbeat(Mutex<Option<Instant>>);

impl Heartbeat {
    pub(crate) fn beat(&self) {
        *self.0.lock().expect("heartbeat mutex poisoned") = Some(Instant::now());
    }

    pub(crate) fn live(&self) -> bool {
        is_fresh(self.0.lock().expect("heartbeat mutex poisoned").map(|at| at.elapsed()))
    }
}

/// Heartbeat from the serving runtime until `cancel` (wedged runtime → `/livez` fails)
pub(crate) async fn beat(
    status: Arc<DaemonStatus>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => status.beat(),
            () = cancel.cancelled() => return Ok(()),
        }
    }
}

/// `None` = never beaten = still starting (k8s covers that window with its own startup delay)
fn is_fresh(age: Option<Duration>) -> bool {
    age.is_none_or(|age| age < HEARTBEAT_MAX_AGE)
}

/// Called by `metrics::init` before the recorder installs (no sample without a draining listener)
pub(crate) fn bind(endpoint: SocketAddr) -> Result<std::net::TcpListener, IndexerError> {
    let listener = std::net::TcpListener::bind(endpoint).map_err(|e| {
        IndexerError::Metrics(format!("admin endpoint {endpoint} failed to bind: {e}"))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| IndexerError::Metrics(format!("admin endpoint {endpoint}: {e}")))?;
    Ok(listener)
}

/// Own thread + runtime
pub(crate) fn spawn(
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
    // Bound before the recorder installed (`metrics::init`): this loop or a scrape drains all
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
            let scrape = move || crate::metrics::scrape(&handle, &status);
            match tokio::task::spawn_blocking(scrape).await {
                Ok(scrape) => body(StatusCode::OK, EXPOSITION_CONTENT_TYPE, scrape),
                Err(e) => {
                    error!(%e, "rendering the scrape panicked");
                    body(StatusCode::INTERNAL_SERVER_ERROR, PLAIN_CONTENT_TYPE, String::new())
                }
            }
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

/// Prometheus text exposition format
///
/// - Load-bearing: Prometheus >= 3.0 drops a scrape with a blank `Content-Type`
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const PLAIN_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

const JSON_CONTENT_TYPE: &str = "application/json";

fn body(status: StatusCode, content_type: &'static str, payload: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::from(payload)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end over a real socket: bind, accept loop, hyper wiring, routing
    #[tokio::test]
    async fn the_admin_surface_answers_metrics_and_probes() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("zaino.test.total").increment(7);
        });

        let listener = bind("127.0.0.1:0".parse().expect("a loopback socket address parses"))
            .expect("loopback bind succeeds");
        let endpoint = listener.local_addr().expect("a bound listener has an address");
        let network = zcash_protocol::consensus::NetworkType::Regtest;
        tokio::spawn(serve(listener, handle, Arc::new(DaemonStatus::new(network, []))));

        // The accept loop races the connect; retry rather than sleep a fixed guess
        let get = |path: &'static str| async move {
            for _ in 0..50 {
                if let Ok(mut stream) = tokio::net::TcpStream::connect(endpoint).await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let request =
                        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
                    stream.write_all(request.as_bytes()).await.unwrap();
                    let mut response = String::new();
                    stream.read_to_string(&mut response).await.unwrap();
                    return response;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("admin endpoint never accepted a connection on {endpoint}");
        };

        let metrics = get("/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
        assert!(metrics.contains("zaino_test_total 7"), "{metrics}");
        assert!(
            metrics.to_ascii_lowercase().contains("content-type: text/plain; version=0.0.4"),
            "{metrics}"
        );
        assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
        // no indexer booted in this test = sources unpublished
        let readyz = get("/readyz").await;
        assert!(readyz.starts_with("HTTP/1.1 503"), "{readyz}");
        assert!(readyz.ends_with(r#"{"detail":null,"state":"provisioning"}"#), "{readyz}");
        assert!(get("/nope").await.starts_with("HTTP/1.1 404"));
    }

    /// Held port → bind error naming the endpoint
    #[test]
    fn a_port_in_use_fails_the_bind() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind succeeds");
        let endpoint = holder.local_addr().expect("a bound listener has an address");
        let error = bind(endpoint).expect_err("a second bind on a held port fails");
        assert!(error.to_string().contains(&endpoint.to_string()), "{error}");
    }

    #[test]
    fn a_stale_heartbeat_is_not_live() {
        for (case, age, fresh) in [
            ("just beaten", Some(Duration::ZERO), true),
            ("stale = wedged runtime", Some(HEARTBEAT_MAX_AGE), false),
            ("never beaten = still starting", None, true),
        ] {
            assert_eq!(is_fresh(age), fresh, "{case}");
        }
    }
}
