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
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use tracing::{error, info, warn};
use zaino_status::StatusType;

use crate::error::IndexerError;

/// Drains histograms into distributions and commits description writes.
///
/// - Matches the built-in exporter's own default; `render` does no upkeep of its own
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Process status behind `/livez` and `/readyz`, republished by the indexer loop.
///
/// - `Relaxed` both ways: the value is a report, nothing is ordered against it, and a
///   probe reading one 100ms tick stale is still answering the question asked
/// - 0 = `StatusType::Spawning`, pinned by `spawning_is_the_zero_discriminant`
static PROCESS_STATUS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn publish_status(status: StatusType) {
    PROCESS_STATUS.store(status.into(), Ordering::Relaxed);
}

fn process_status() -> StatusType {
    StatusType::from(PROCESS_STATUS.load(Ordering::Relaxed))
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
    let listener = match tokio::net::TcpListener::bind(endpoint).await {
        Ok(listener) => listener,
        Err(e) => return error!(%e, %endpoint, "admin endpoint failed to bind"),
    };
    info!(%endpoint, "admin endpoint started: /metrics, /livez, /readyz");

    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(UPKEEP_INTERVAL);
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });

    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                warn!(%e, "admin connection failed to accept");
                continue;
            }
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let handle = handle.clone();
                async move { Ok::<_, Infallible>(route(&handle, &request)) }
            });
            // Connection errors are the client hanging up mid-scrape; nothing to do
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

fn route(
    handle: &PrometheusHandle,
    request: &Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    match request.uri().path() {
        // Sampled here rather than on a timer: a scrape is the only reader, and a
        // timer publishes RSS nobody asked for while idle
        "/metrics" => {
            crate::metrics::collect_process_metrics();
            body(StatusCode::OK, handle.render())
        }
        // Live but not ready is the normal state while syncing; only an unrecoverable
        // component asks to be restarted
        "/livez" => probe(process_status().is_live()),
        "/readyz" => probe(process_status().is_ready()),
        _ => body(StatusCode::NOT_FOUND, String::new()),
    }
}

fn probe(passing: bool) -> Response<Full<Bytes>> {
    match passing {
        true => body(StatusCode::OK, "ok".to_string()),
        false => body(StatusCode::SERVICE_UNAVAILABLE, "unavailable".to_string()),
    }
}

fn body(status: StatusCode, payload: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
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
        assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
        assert!(get("/readyz").await.starts_with("HTTP/1.1 503"));

        publish_status(StatusType::Ready);
        assert!(get("/readyz").await.starts_with("HTTP/1.1 200"));
        assert!(get("/nope").await.starts_with("HTTP/1.1 404"));
    }

    /// - Syncing is live but not ready: k8s must withhold traffic without restarting
    #[test]
    fn probe_codes_follow_the_status() {
        for (status, live, ready) in [
            (StatusType::Spawning, true, false),
            (StatusType::Syncing, true, false),
            (StatusType::Ready, true, true),
            (StatusType::CriticalError, false, false),
        ] {
            publish_status(status);
            assert_eq!(process_status().is_live(), live, "{status:?} liveness");
            assert_eq!(process_status().is_ready(), ready, "{status:?} readiness");
        }
    }
}
