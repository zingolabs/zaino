//! Admin surface: `/metrics`, `/livez`, `/readyz`, on a runtime of its own.
//!
//! - Own thread + current-thread runtime: the serving runtime is the one that
//!   saturates, and a probe answered from there measures its queue, not the process
//! - k8s restarts a pod whose liveness probe times out, so a shared runtime turns a
//!   busy indexer into a restart loop
//! - `install_recorder` (not `install`) hands us the render handle and, with it, the
//!   upkeep obligation — see [`UPKEEP_INTERVAL`]

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use tracing::{error, info, warn};
use zaino_status::StatusType;

use crate::error::IndexerError;

/// Bounds `AtomicBucket` growth between scrapes.
///
/// - `render` drains histograms itself, so this is not what keeps a scrape correct —
///   it is what stops samples piling up when nothing is scraping
/// - Matches the built-in exporter's own default
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Concurrent admin connections; the rest wait in the accept backlog.
///
/// - Unbounded `spawn` per connection is an fd exhaustion of the *whole process*,
///   and this endpoint may bind a non-private address by design
const MAX_CONNECTIONS: usize = 32;

/// How stale [`PROCESS_STATUS`] may be before `/livez` fails.
///
/// - Indexer republishes every 100ms; a wedged runtime stops republishing while the
///   admin thread keeps answering, so without this a hung pod reports live forever
/// - Generous, since the point is to catch a wedge, not a scheduling hiccup
const STATUS_MAX_AGE: Duration = Duration::from_secs(30);

/// Process status behind `/livez` and `/readyz`, republished by the indexer loop.
///
/// - `Relaxed` both ways: the value is a report, nothing is ordered against it, and a
///   probe reading one 100ms tick stale is still answering the question asked
/// - 0 = `StatusType::Spawning`, pinned by `spawning_is_the_zero_discriminant`
static PROCESS_STATUS: AtomicUsize = AtomicUsize::new(0);

/// Millis since [`epoch`] at the last [`publish_status`]; 0 = never published.
static PROCESS_STATUS_AT: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

pub(crate) fn publish_status(status: StatusType) {
    PROCESS_STATUS.store(status.into(), Ordering::Relaxed);
    PROCESS_STATUS_AT.store(
        epoch().elapsed().as_millis().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

fn process_status() -> StatusType {
    StatusType::from(PROCESS_STATUS.load(Ordering::Relaxed))
}

/// Whether the indexer is still reporting.
///
/// - Before the first publish the indexer is still starting, so a stale reading is
///   not yet meaningful; k8s covers that window with its own startup delay
fn status_is_fresh() -> bool {
    match PROCESS_STATUS_AT.load(Ordering::Relaxed) {
        0 => true,
        at => epoch().elapsed().saturating_sub(Duration::from_millis(at)) < STATUS_MAX_AGE,
    }
}

/// Start the admin listener on its own thread.
///
/// - Thread, not `tokio::spawn`: the isolation is the point
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
    // Before the bind, so a bind failure still leaves upkeep running rather than
    // letting samples pile up behind a listener that never came up
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
        // Downs the probes too, so k8s will restart the pod on this
        Err(e) => return error!(%e, %endpoint, "admin endpoint failed to bind"),
    };
    info!(%endpoint, "admin endpoint started: /metrics, /livez, /readyz");

    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        // Held for the connection's life; the cap is what bounds fds and tasks
        let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Backoff, not `continue`: tokio clears readiness only on WouldBlock,
                // so EMFILE would spin this loop at 100% CPU during an fd shortage
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
            // `timer` is load-bearing: without one hyper silently drops its own 30s
            // header-read timeout, leaving the listener open to slowloris
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
        // Off the admin runtime: `render` walks every handle and drains every
        // histogram, and this thread also answers the probes
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
        // Live but not ready is the normal state while syncing; only an unrecoverable
        // component, or an indexer that stopped reporting, asks to be restarted
        "/livez" => probe(process_status().is_live() && status_is_fresh()),
        "/readyz" => probe(process_status().is_ready() && status_is_fresh()),
        _ => body(StatusCode::NOT_FOUND, PLAIN_CONTENT_TYPE, String::new()),
    }
}

fn probe(passing: bool) -> Response<Full<Bytes>> {
    match passing {
        true => body(StatusCode::OK, PLAIN_CONTENT_TYPE, "ok".to_string()),
        false => body(
            StatusCode::SERVICE_UNAVAILABLE,
            PLAIN_CONTENT_TYPE,
            "unavailable".to_string(),
        ),
    }
}

/// Prometheus text exposition format, version pinned in the type.
///
/// - Load-bearing: Prometheus >= 3.0 drops a scrape whose response carries no
///   `Content-Type` ("non-compliant scrape target sending blank Content-Type") unless the
///   *server* opts in with `fallback_scrape_protocol`
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

    /// - [`PROCESS_STATUS`] starts at 0 and must mean `Spawning`, not whatever variant
    ///   a reorder puts first. Not asserted through `process_status`: the other tests
    ///   write that global, and under `cargo test` they share a process
    #[test]
    fn spawning_is_the_zero_discriminant() {
        assert_eq!(usize::from(StatusType::Spawning), 0);
    }

    /// End-to-end over a real socket: routing, status codes and the rendered scrape.
    ///
    /// - `serve` is the whole surface; testing `route` alone would not catch a bind,
    ///   an accept loop or a hyper wiring mistake
    #[tokio::test]
    async fn the_admin_surface_answers_metrics_and_both_probes() {
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

        publish_status(StatusType::Syncing);
        let metrics = get("/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
        assert!(metrics.contains("zaino_test_total 7"), "{metrics}");
        // Prometheus >= 3.0 drops a scrape carrying no `Content-Type` — the header the
        // exporter's own listener sets and a hand-rolled hyper service does not
        assert!(
            metrics
                .to_ascii_lowercase()
                .contains("content-type: text/plain; version=0.0.4"),
            "{metrics}"
        );
        assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
        assert!(get("/readyz").await.starts_with("HTTP/1.1 503"));

        publish_status(StatusType::Ready);
        assert!(get("/readyz").await.starts_with("HTTP/1.1 200"));
        assert!(get("/nope").await.starts_with("HTTP/1.1 404"));
    }

    /// - Syncing is live but not ready: k8s must withhold traffic without restarting
    /// - Through `probe`, not `publish_status`: the global is shared with the
    ///   end-to-end test, and under `cargo test` the two run in one process
    #[test]
    fn probe_codes_follow_the_status() {
        let code = |passing| probe(passing).status();
        let expected = |passing| match passing {
            true => StatusCode::OK,
            false => StatusCode::SERVICE_UNAVAILABLE,
        };
        for (status, live, ready) in [
            (StatusType::Spawning, true, false),
            (StatusType::Syncing, true, false),
            (StatusType::Ready, true, true),
            (StatusType::Busy, true, true),
            (StatusType::Offline, false, false),
            (StatusType::CriticalError, false, false),
        ] {
            assert_eq!(code(status.is_live()), expected(live), "{status:?} /livez");
            assert_eq!(
                code(status.is_ready()),
                expected(ready),
                "{status:?} /readyz"
            );
        }
    }
}
