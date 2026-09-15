//! Admin surface: `/metrics`, `/livez`, `/health`, on a runtime of its own.
//!
//! - Own thread + current-thread runtime: a probe answered from the saturated serving runtime
//!   measures its queue, and a timed-out liveness probe gets the pod killed
//! - `/metrics` = quantities only; modes & flags go on `/health`
//! - No `/readyz` yet: readiness arrives with per-component `ComponentStatus` reporting

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
use zaino_mempool::snapshot::MempoolCompleteness;
use zaino_state::{FinalisedStateMode, IndexHealth};

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

#[derive(Clone, Copy)]
struct Heartbeat {
    at: Instant,
    health: Option<IndexHealth>,
}

static HEARTBEAT: Mutex<Option<Heartbeat>> = Mutex::new(None);

/// Called by the indexer loop every tick; `health` = `None` before the service exists
pub(crate) fn publish(health: Option<IndexHealth>) {
    // Poison-tolerant: the lock only guards a `Copy` swap
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Heartbeat {
        at: Instant::now(),
        health,
    });
}

fn latest() -> Option<Heartbeat> {
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Never published = still starting (k8s covers that window with its own startup delay)
fn is_fresh(heartbeat: Option<Heartbeat>) -> bool {
    heartbeat.is_none_or(|heartbeat| heartbeat.at.elapsed() < HEARTBEAT_MAX_AGE)
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
    info!(%endpoint, "admin endpoint started: /metrics, /livez, /health");

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
        "/livez" => match is_fresh(latest()) {
            true => body(StatusCode::OK, PLAIN_CONTENT_TYPE, "ok".to_string()),
            false => body(
                StatusCode::SERVICE_UNAVAILABLE,
                PLAIN_CONTENT_TYPE,
                "unavailable".to_string(),
            ),
        },
        "/health" => health(latest()),
        _ => body(StatusCode::NOT_FOUND, PLAIN_CONTENT_TYPE, String::new()),
    }
}

fn health(heartbeat: Option<Heartbeat>) -> Response<Full<Bytes>> {
    let (code, payload) = health_report(heartbeat);
    body(code, JSON_CONTENT_TYPE, payload)
}

fn health_report(heartbeat: Option<Heartbeat>) -> (StatusCode, String) {
    let code = match is_fresh(heartbeat) {
        true => StatusCode::OK,
        false => StatusCode::SERVICE_UNAVAILABLE,
    };
    let payload = match heartbeat.and_then(|heartbeat| heartbeat.health) {
        Some(health) => format!(
            r#"{{"finalised_state_mode":"{}","mempool_completeness":"{}","accumulator_rebuild_active":{}}}"#,
            finalised_state_mode(health.finalised_state_mode),
            mempool_completeness(health.mempool_completeness),
            health.accumulator_rebuild_active,
        ),
        None => r#"{"finalised_state_mode":null,"mempool_completeness":null,"accumulator_rebuild_active":null}"#
            .to_string(),
    };
    (code, payload)
}

// Wire names owned here, exhaustively: a new variant fails to compile rather than serialise blind
fn finalised_state_mode(mode: FinalisedStateMode) -> &'static str {
    match mode {
        FinalisedStateMode::EphemeralConfigured => "ephemeral(configured)",
        FinalisedStateMode::EphemeralSyncing => "ephemeral(syncing)",
        FinalisedStateMode::EphemeralMigrating => "ephemeral(migrating)",
        FinalisedStateMode::Persistent => "persistent",
    }
}

fn mempool_completeness(completeness: MempoolCompleteness) -> &'static str {
    match completeness {
        MempoolCompleteness::Complete => "complete",
        MempoolCompleteness::IncompleteCapacityLimited => "incomplete(capacity_limited)",
        MempoolCompleteness::IncompletePendingMetadata => "incomplete(pending_metadata)",
        MempoolCompleteness::IncompleteSourceError => "incomplete(source_error)",
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
    async fn the_admin_surface_answers_metrics_livez_and_health() {
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
        assert!(get("/health").await.starts_with("HTTP/1.1 200"));
        assert!(get("/readyz").await.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn health_serialises_the_published_snapshot() {
        let heartbeat = Heartbeat {
            at: Instant::now(),
            health: Some(IndexHealth {
                finalised_state_mode: FinalisedStateMode::EphemeralSyncing,
                mempool_completeness: MempoolCompleteness::IncompleteCapacityLimited,
                accumulator_rebuild_active: true,
            }),
        };
        assert_eq!(
            health_report(Some(heartbeat)),
            (
                StatusCode::OK,
                r#"{"finalised_state_mode":"ephemeral(syncing)","mempool_completeness":"incomplete(capacity_limited)","accumulator_rebuild_active":true}"#
                    .to_string()
            )
        );
    }

    /// - Stale heartbeat = wedged indexer runtime → 503 on both probes' source
    #[test]
    fn a_stale_heartbeat_is_unavailable() {
        let Some(at) = Instant::now().checked_sub(HEARTBEAT_MAX_AGE + Duration::from_secs(1))
        else {
            return; // monotonic clock younger than the max age (fresh boot)
        };
        let stale = Some(Heartbeat { at, health: None });
        assert!(!is_fresh(stale));
        assert_eq!(health_report(stale).0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(is_fresh(None), "never published = still starting");
    }
}
