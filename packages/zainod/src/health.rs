//! HTTP health endpoint for Zaino.
//!
//! Route names follow the Kubernetes API-server convention (`/livez`,
//! `/readyz`); the config shape mirrors Zebra's `health` component (a
//! `[health] listen_addr` section) so operators configure both daemons the same
//! way. The routes are driven by the indexer's existing status model
//! ([`zaino_common::probing`]):
//!
//! - `GET /livez`  — `200` while the indexer is live (incl. syncing), `503`
//!   once it is [`Offline`]/[`CriticalError`]. The liveness probe.
//! - `GET /readyz` — `200` only once the indexer is ready to serve (at tip),
//!   `503` while still syncing. The readiness probe.
//!
//! The readiness gate keeps a half-synced indexer out of a Service without
//! letting a slow start be mistaken for a wedged process. Like Zebra, it is
//! opt-in — disabled until `health.listen_addr` is set.
//!
//! [`Offline`]: zaino_common::status::StatusType::Offline
//! [`CriticalError`]: zaino_common::status::StatusType::CriticalError

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use http_body_util::Full;
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::{info, warn};
use zaino_common::probing::{Liveness, Readiness};
use zaino_state::SyncProgress;

use crate::error::IndexerError;

/// Readiness probe combining process liveness with tip-proximity.
///
/// `/livez` is driven by the subscriber's aggregated status ([`Liveness`]).
/// `/readyz` additionally requires the index to be close to the network tip and
/// not stale — gating on [`SyncProgress`] rather than the raw status, which the
/// sync loop flips to `Syncing` at the top of every steady-state iteration and
/// would otherwise make readiness flap. Mirrors Zebra's `/ready` semantics.
pub struct IndexerProbe<S> {
    subscriber: S,
    progress: SyncProgress,
    max_blocks_behind: u64,
    max_tip_age: Duration,
}

impl<S> IndexerProbe<S> {
    /// Wraps `subscriber` with tip-proximity readiness thresholds.
    pub fn new(
        subscriber: S,
        progress: SyncProgress,
        max_blocks_behind: u64,
        max_tip_age: Duration,
    ) -> Self {
        IndexerProbe {
            subscriber,
            progress,
            max_blocks_behind,
            max_tip_age,
        }
    }
}

impl<S: Liveness> Liveness for IndexerProbe<S> {
    fn is_live(&self) -> bool {
        self.subscriber.is_live()
    }
}

impl<S: Liveness> Readiness for IndexerProbe<S> {
    fn is_ready(&self) -> bool {
        self.subscriber.is_live()
            && self.progress.blocks_behind() <= self.max_blocks_behind
            && self.progress.tip_age() <= self.max_tip_age
    }
}

/// Binds the health endpoint and spawns its accept loop.
///
/// Binding happens synchronously (before returning) so `EADDRINUSE` / `EACCES`
/// propagate to the caller instead of being swallowed inside the spawned task —
/// the same rationale as [`TonicServer::spawn`](zaino_serve::server::grpc).
pub async fn spawn<P>(endpoint: SocketAddr, probe: P) -> Result<JoinHandle<()>, IndexerError>
where
    P: Liveness + Readiness + Send + Sync + 'static,
{
    let listener = bind(endpoint).await?;
    info!(%endpoint, "HTTP health endpoint started");
    Ok(serve(listener, probe))
}

/// Binds the health endpoint's TCP listener, mapping bind failures to
/// [`IndexerError`].
async fn bind(endpoint: SocketAddr) -> Result<TcpListener, IndexerError> {
    TcpListener::bind(endpoint)
        .await
        .map_err(|e| IndexerError::MiscIndexerError(format!("health endpoint bind failed: {e}")))
}

/// Spawns the accept loop serving `/livez` and `/readyz` from `probe`.
fn serve<P>(listener: TcpListener, probe: P) -> JoinHandle<()>
where
    P: Liveness + Readiness + Send + Sync + 'static,
{
    let probe = Arc::new(probe);
    tokio::task::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _peer)) => stream,
                Err(e) => {
                    warn!("health endpoint accept error: {e}");
                    continue;
                }
            };
            let probe = Arc::clone(&probe);
            tokio::task::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let probe = Arc::clone(&probe);
                    async move { Ok::<_, Infallible>(route(probe.as_ref(), &req)) }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    warn!("health endpoint connection error: {e}");
                }
            });
        }
    })
}

/// Maps a request to a response: `/livez` → liveness, `/readyz` → readiness,
/// anything else → `404`.
fn route<P>(probe: &P, req: &Request<Incoming>) -> Response<Full<Bytes>>
where
    P: Liveness + Readiness,
{
    let (status, body): (StatusCode, &'static str) = match (req.method(), req.uri().path()) {
        (&Method::GET, "/livez") => probe_response(probe.is_live()),
        (&Method::GET, "/readyz") => probe_response(probe.is_ready()),
        _ => (StatusCode::NOT_FOUND, "not found\n"),
    };

    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        // `Response::builder` only errors on an invalid status/header, none of
        // which are reachable from the literals above; fall back rather than
        // panic to honour the no-`unwrap` rule.
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"error\n"))))
}

/// `200 ok` when the probe is satisfied, `503 unavailable` otherwise.
fn probe_response(ok: bool) -> (StatusCode, &'static str) {
    if ok {
        (StatusCode::OK, "ok\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Test probe with independently settable liveness / readiness, mirroring
    /// the `StatusType` states health checks care about without coupling to its
    /// mapping (which is covered in `zaino-common`).
    #[derive(Clone)]
    struct FakeProbe {
        live: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
    }

    impl FakeProbe {
        fn new(live: bool, ready: bool) -> Self {
            FakeProbe {
                live: Arc::new(AtomicBool::new(live)),
                ready: Arc::new(AtomicBool::new(ready)),
            }
        }
    }

    impl Liveness for FakeProbe {
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
    }

    impl Readiness for FakeProbe {
        fn is_ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
    }

    /// Issues `GET {path}` over a fresh connection and returns the status code.
    async fn get_status(addr: SocketAddr, path: &str) -> u16 {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to health endpoint");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let text = String::from_utf8_lossy(&response);
        text.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status code in response line")
    }

    /// Liveness-only stub so `IndexerProbe` readiness can be exercised against
    /// a controllable `SyncProgress` without a real backend.
    struct StubLiveness(bool);
    impl Liveness for StubLiveness {
        fn is_live(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn readiness_gates_on_tip_proximity() {
        let progress = SyncProgress::new();
        let probe = IndexerProbe::new(
            StubLiveness(true),
            progress.clone(),
            2,
            Duration::from_secs(300),
        );

        // No tip observed yet: live but fully behind -> not ready.
        assert!(probe.is_live());
        assert!(!probe.is_ready());

        // Within tolerance (1 <= 2): ready.
        progress.record_network_tip(1_000);
        progress.record_synced(999);
        assert!(probe.is_ready());

        // Fell behind (11 > 2): not ready, still live.
        progress.record_network_tip(1_010);
        assert!(!probe.is_ready());
        assert!(probe.is_live());

        // Caught up: ready again.
        progress.record_synced(1_010);
        assert!(probe.is_ready());
    }

    #[test]
    fn readiness_fails_when_not_live() {
        let progress = SyncProgress::new();
        progress.record_network_tip(100);
        progress.record_synced(100); // caught up
        let probe = IndexerProbe::new(StubLiveness(false), progress, 2, Duration::from_secs(300));
        assert!(!probe.is_live());
        assert!(!probe.is_ready()); // not ready despite being at tip
    }

    #[test]
    fn readiness_fails_when_tip_is_stale() {
        let progress = SyncProgress::new();
        progress.record_network_tip(100);
        progress.record_synced(100); // caught up (blocks_behind == 0)
                                     // Zero tolerance for tip age: any elapsed time since last sync fails.
        let probe = IndexerProbe::new(StubLiveness(true), progress, 2, Duration::ZERO);
        assert!(!probe.is_ready());
    }

    /// Drives the health routes across the three states a probe distinguishes.
    // multi_thread: the server accept loop and the in-test client must make
    // progress concurrently on the same runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn health_routes_reflect_probe_state() {
        async fn assert_states(live: bool, ready: bool, expect_live: u16, expect_ready: u16) {
            let listener = bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind ephemeral port");
            let addr = listener.local_addr().expect("local_addr");
            let handle = serve(listener, FakeProbe::new(live, ready));

            assert_eq!(get_status(addr, "/livez").await, expect_live);
            assert_eq!(get_status(addr, "/readyz").await, expect_ready);
            assert_eq!(get_status(addr, "/nope").await, 404);

            handle.abort();
        }

        // Syncing: live but not ready.
        assert_states(true, false, 200, 503).await;
        // Ready: both serve 200.
        assert_states(true, true, 200, 200).await;
        // CriticalError / Offline: liveness fails too.
        assert_states(false, false, 503, 503).await;
    }
}
