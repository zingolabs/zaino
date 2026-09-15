//! Admin surface: `/metrics`, `/livez`, on a runtime of its own.
//!
//! - Own thread + current-thread runtime: a probe answered from the saturated serving runtime
//!   measures its queue, and a timed-out liveness probe gets the pod killed
//! - `/readyz` TODO: per-component `ComponentStatus` (see `usage.md`)

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use tracing::{error, info, warn};

use crate::error::IndexerError;

/// Bounds `AtomicBucket` growth between scrapes (`render` drains itself; this covers no scraper)
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Concurrent admin connections; the rest wait in the accept backlog
///
/// - Unbounded `spawn` per connection = fd exhaustion of the whole process (may bind non-private)
const MAX_CONNECTIONS: usize = 32;

/// Heartbeat age past which `/livez` fails
///
/// - Indexer republishes every 100ms; a wedged runtime stops while this thread keeps answering
const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(30);

static HEARTBEAT: Mutex<Option<Instant>> = Mutex::new(None);

/// Called by the indexer loop every tick
pub(crate) fn heartbeat() {
    // Poison-tolerant: the lock only guards a `Copy` swap
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
}

/// Supervisor restart: back to "still starting" (the respawn stops the loop, may outlast 30s)
pub(crate) fn clear_heartbeat() {
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

fn last_heartbeat() -> Option<Instant> {
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Never beaten = still starting (k8s covers that window with its own startup delay)
fn is_fresh(heartbeat: Option<Instant>) -> bool {
    heartbeat.is_none_or(|at| at.elapsed() < HEARTBEAT_MAX_AGE)
}

/// Start the admin listener on its own thread.
///
/// - A bind failure downs telemetry, not the indexer, so it logs rather than returns
pub(crate) fn spawn(endpoint: SocketAddr, handle: PrometheusHandle) -> Result<(), IndexerError> {
    std::thread::Builder::new()
        .name("zaino-admin".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => return error!(%e, "admin runtime failed to build; no /metrics or probes"),
            };
            runtime.block_on(serve(endpoint, handle));
        })
        .map_err(|e| IndexerError::MetricsError(format!("failed to spawn admin thread: {e}")))?;
    Ok(())
}

async fn serve(endpoint: SocketAddr, handle: PrometheusHandle) {
    // Before the bind: a failed bind must not leave samples piling up
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(UPKEEP_INTERVAL);
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });

    let listener = match tokio::net::TcpListener::bind(endpoint).await {
        Ok(listener) => listener,
        Err(e) => return error!(%e, %endpoint, "admin endpoint failed to bind"),
    };
    info!(%endpoint, "admin endpoint started: /metrics, /livez");

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
        let handle = handle.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                let handle = handle.clone();
                async move { Ok::<_, Infallible>(route(&handle, &request).await) }
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
    request: &Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    match request.uri().path() {
        // Off the admin runtime: `render` walks every handle, and this thread answers probes
        "/metrics" => {
            let handle = handle.clone();
            match tokio::task::spawn_blocking(move || {
                crate::metrics::collect_process_metrics();
                handle.render()
            })
            .await
            {
                Ok(scrape) => body(StatusCode::OK, EXPOSITION_CONTENT_TYPE, scrape),
                Err(e) => {
                    error!(%e, "rendering the scrape panicked");
                    body(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        PLAIN_CONTENT_TYPE,
                        String::new(),
                    )
                }
            }
        }
        "/livez" => match is_fresh(last_heartbeat()) {
            true => body(StatusCode::OK, PLAIN_CONTENT_TYPE, "ok".to_string()),
            false => body(
                StatusCode::SERVICE_UNAVAILABLE,
                PLAIN_CONTENT_TYPE,
                "unavailable".to_string(),
            ),
        },
        _ => body(StatusCode::NOT_FOUND, PLAIN_CONTENT_TYPE, String::new()),
    }
}

/// Prometheus text exposition format
///
/// - Load-bearing: Prometheus >= 3.0 drops a scrape with a blank `Content-Type`
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const PLAIN_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

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
    async fn the_admin_surface_answers_metrics_and_livez() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("zaino.test.total").increment(7);
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        drop(listener);
        tokio::spawn(serve(endpoint, handle));

        // Bind races the spawn; retry rather than sleep a fixed guess
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
            metrics
                .to_ascii_lowercase()
                .contains("content-type: text/plain; version=0.0.4"),
            "{metrics}"
        );
        assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
        assert!(get("/readyz").await.starts_with("HTTP/1.1 404"));
    }

    /// - Stale heartbeat = wedged indexer runtime
    #[test]
    fn a_stale_heartbeat_is_not_live() {
        let Some(at) = Instant::now().checked_sub(HEARTBEAT_MAX_AGE + Duration::from_secs(1))
        else {
            return; // monotonic clock younger than the max age (fresh boot)
        };
        assert!(!is_fresh(Some(at)));
        assert!(is_fresh(Some(Instant::now())));
        assert!(is_fresh(None), "never beaten = still starting");
    }
}
