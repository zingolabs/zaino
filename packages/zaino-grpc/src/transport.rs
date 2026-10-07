//! [`GrpcService`]: hyper-util's HTTP/2 server around the path dispatch (`service.rs`)
//! (`service.bind().await?` at boot, then `tokio::spawn(bound.run(cancel))`)
//!
//! Stack, outermost first:
//!
//! ```text
//!   accept ─ connection caps ─ PROXY header ─ TLS ─ h2 conn ─ metrics ─ admission ─ dispatch
//! ```
//!
//! - hyper, not `tonic::transport::Server` (own accept loop: over-cap closed before a task
//!   exists; h2 settings per connection)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn, Instrument as _, Span};
use zaino_persistence::{MapRead, SequenceRead};
use zaino_source::ChainDataSource;

use crate::admission::{Admission, Class, Permits};
use crate::client::TrustedProxies;
use crate::connections::{ConnectionCaps, Reserved};
use crate::limits::ReadLanes;
use crate::observe::Measured;
use crate::report::{self, Held, INTERVAL};
use crate::service::{Dispatch, Routes};
use crate::{emit, GrpcLimits};

/// Per-stream send buffer (flow control paces from here: a slow wallet backs up itself, not the
/// server)
const SEND_BUFFER: usize = 64 * 1024;

/// Unsent bytes the kernel queues per socket before it reports the socket unwritable
///
/// - h2 then picks the next frame, not the kernel: a unary reply overtakes a range already queued
///   on the same connection (4 MiB of autotuned `tcp_wmem` = seconds on a mobile link)
/// - bounds kernel memory per connection
#[cfg(any(target_os = "linux", target_os = "android"))]
const NOTSENT_LOWAT: u32 = 128 * 1024;

/// `accept()` retry after a resource error (`EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`): doubles
/// from `MIN` to `MAX`, reset by the next accepted socket
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// TLS handshake deadline (a silent client holds a connection slot at most this long)
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle-connection ping cadence + pong deadline
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Locally-reset streams per peer before the connection closes (CVE-2023-44487)
const RAPID_RESET_LIMIT: usize = 128;

/// `CompactTxStreamer` endpoint: enabled [`Routes`] on one listener, bounded by [`GrpcLimits`]
pub struct GrpcService<S: ChainDataSource, V> {
    dispatch: Dispatch<S, V>,
    bind: SocketAddr,
    limits: GrpcLimits,
    proxies: TrustedProxies,
    tls: Option<crate::Tls>,
}

#[derive(Debug, thiserror::Error)]
pub enum GrpcServeError {
    #[error("gRPC server error: {0}")]
    Serve(String),
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> GrpcService<S, V> {
    pub fn new(routes: Routes<S, V>, bind: SocketAddr, limits: GrpcLimits) -> Self {
        let dispatch = Dispatch::new(routes, ReadLanes::new(&limits));
        Self { dispatch, bind, limits, proxies: TrustedProxies::default(), tls: None }
    }

    /// Terminates TLS on the listener (else plaintext h2c, for a TLS-terminating proxy in front)
    pub fn with_tls(mut self, tls: crate::Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Peers that front this server and name each client in a PROXY header (v1 or v2)
    ///
    /// - `max_connections_per_ip` then counts the named client, not the proxy
    /// - a trusted peer sending no header is dropped
    pub fn with_trusted_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.proxies = proxies;
        self
    }
}

fn http2(limits: &GrpcLimits) -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new()).http2_only();
    builder
        .http2()
        // keepalive pings need a clock (hyper has none)
        .timer(TokioTimer::new())
        .max_concurrent_streams(limits.max_streams_per_connection.get())
        .max_send_buf_size(SEND_BUFFER)
        .keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .max_local_error_reset_streams(RAPID_RESET_LIMIT);

    builder
}

/// [`GrpcService`] holding its socket, ready to [`run`](Self::run)
pub struct BoundGrpcService<S: ChainDataSource, V> {
    server: GrpcService<S, V>,
    listener: TcpListener,
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> GrpcService<S, V> {
    /// Awaited at boot, before spawning `run` (EADDRINUSE = boot failure, not a serve-loop
    /// failure (#1081))
    pub async fn bind(self) -> Result<BoundGrpcService<S, V>, GrpcServeError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|e| GrpcServeError::Serve(format!("bind failed: {e}")))?;
        Ok(BoundGrpcService { server: self, listener })
    }
}

/// Per-connection service stack (module doc)
type Stack<S, V> = TowerToHyperService<Measured<Admission<Dispatch<S, V>>>>;

/// What every connection task shares
struct Shared<S: ChainDataSource, V> {
    caps: ConnectionCaps,
    proxies: TrustedProxies,
    tls: Option<tokio_rustls::TlsAcceptor>,
    http2: auto::Builder<TokioExecutor>,
    dispatch: Dispatch<S, V>,
    permits: Permits,
    limits: GrpcLimits,
    cancel: CancellationToken,
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Shared<S, V> {
    /// Permits and connections held now, each against its cap
    fn held(&self) -> Held {
        Held {
            streams: self.permits.held(Class::Work, self.limits.max_streams.get()),
            subscriptions: self
                .permits
                .held(Class::Subscription, self.limits.max_subscriptions.get()),
            connections: self.caps.held(self.limits.max_connections.get()),
        }
    }
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> BoundGrpcService<S, V> {
    /// Accept + serve until `cancel`, then drain: listener closed, every connection GOAWAY'd,
    /// returns once they all finish or `drain_timeout` passes (the rest dropped)
    ///
    /// - never ends on an accept error: a resource error (fd limit, memory) backs off and
    ///   retries, the listener recovering as connections close
    pub async fn run(self, cancel: CancellationToken) -> Result<(), GrpcServeError> {
        let drain_timeout = self.server.limits.drain_timeout;
        let connections = TaskTracker::new();
        self.accept(&cancel, &connections).await;
        connections.close();
        let open = connections.len();
        if open == 0 || drain_timeout.is_zero() {
            return Ok(());
        }
        info!(open, timeout = ?drain_timeout, "Draining connections");
        if tokio::time::timeout(drain_timeout, connections.wait()).await.is_err() {
            warn!(open = connections.len(), timeout = ?drain_timeout, "Drain timed out, dropping");
        }
        Ok(())
    }

    /// Until `cancel`, each connection on `connections`; returning drops the listener
    async fn accept(self, cancel: &CancellationToken, connections: &TaskTracker) {
        let Self { server, listener } = self;
        if let Some(tls) = &server.tls {
            tokio::spawn(tls.reload(cancel.clone()).instrument(Span::current()));
        }
        let shared = Arc::new(Shared {
            caps: ConnectionCaps::new(&server.limits),
            proxies: server.proxies,
            tls: server.tls.as_ref().map(crate::Tls::acceptor),
            http2: http2(&server.limits),
            dispatch: server.dispatch,
            permits: Permits::new(&server.limits),
            limits: server.limits,
            cancel: cancel.clone(),
        });
        let mut backoff = ACCEPT_BACKOFF_MIN;
        let mut summaries = tokio::time::interval_at(Instant::now() + INTERVAL, INTERVAL);
        summaries.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut window_opened = Instant::now();

        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
                now = summaries.tick() => {
                    report::summarise(now - window_opened, shared.held());
                    window_opened = now;
                    continue;
                }
            };

            let (socket, peer) = match accepted {
                Ok(accepted) => {
                    backoff = ACCEPT_BACKOFF_MIN;
                    accepted
                }
                Err(error) if peer_gone(&error) => continue,
                Err(error) => {
                    emit::accept_failed();
                    warn!(%error, retry_in = ?backoff, "Accept failed");
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                    continue;
                }
            };

            // Over a cap: dropped here (refusal costs no task)
            let Some(reserved) = shared.caps.reserve() else {
                continue;
            };
            if let Err(error) = tune(&socket) {
                debug!(%error, %peer, "Dropping a socket that refused its options");
                continue;
            }

            let connection = Arc::clone(&shared).connection(socket, peer, reserved);
            connections.spawn(connection.instrument(Span::current()));
        }
    }
}

/// Failed on the peer's side, not the listener's (no backoff)
fn peer_gone(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(
        error.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

/// `TCP_NODELAY` (a reply's last segment never waits on delayed ACK), plus `NOTSENT_LOWAT`
fn tune(socket: &TcpStream) -> std::io::Result<()> {
    socket.set_nodelay(true)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    socket2::SockRef::from(socket).set_tcp_notsent_lowat(NOTSENT_LOWAT)?;
    Ok(())
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Shared<S, V> {
    /// Names the client, applies its cap, then serves HTTP/2 until close or `cancel`
    async fn connection(
        self: Arc<Self>,
        mut socket: TcpStream,
        peer: SocketAddr,
        reserved: Reserved,
    ) {
        let peer_ip = peer.ip().to_canonical();
        if !self.proxies.trusts(peer_ip) {
            let Some(_connection) = self.caps.admit(reserved, peer_ip) else {
                return;
            };
            return self.secure(socket, peer).await;
        }

        let header = tokio::time::timeout(
            crate::client::HEADER_TIMEOUT,
            crate::client::read_header(&mut socket),
        )
        .await
        .unwrap_or(Err(crate::client::HeaderError::Timeout));
        let (source, read_ahead) = match header {
            Ok(read) => read,
            Err(error) => {
                warn!(%error, %peer, "Dropping a trusted proxy's connection");
                return;
            }
        };
        let client = source.map_or(peer_ip, |source| source.to_canonical());
        let Some(_connection) = self.caps.admit(reserved, client) else {
            return;
        };

        // Bytes past the header = client's HTTP/2 stream: replayed first
        let (reader, writer) = socket.into_split();
        let io = tokio::io::join(std::io::Cursor::new(read_ahead).chain(reader), writer);
        self.secure(io, peer).await;
    }

    /// TLS handshake when configured (bounded by [`TLS_HANDSHAKE_TIMEOUT`]), then [`Self::serve`]
    async fn secure<I>(&self, io: I, peer: SocketAddr)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let Some(acceptor) = &self.tls else {
            return self.serve(io).await;
        };
        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(io)).await {
            Ok(Ok(stream)) => self.serve(stream).await,
            // scanners and plaintext clients: routine, not an operator signal
            Ok(Err(error)) => debug!(%error, %peer, "TLS handshake failed"),
            Err(_) => debug!(%peer, "TLS handshake timed out"),
        }
    }

    /// Until the peer closes, `cancel` (graceful), or a stream holds unread data past
    /// `stall_timeout` (dropped: graceful would wait on the very stream that stalled)
    async fn serve<I>(&self, io: I)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let stalls = Arc::new(crate::stall::Watch::default());
        let service: Stack<S, V> = TowerToHyperService::new(Measured::new(Admission::new(
            self.dispatch.clone(),
            self.permits.clone(),
            Arc::clone(&stalls),
        )));
        let serving = self.http2.serve_connection(TokioIo::new(io), service);
        let mut serving = std::pin::pin!(serving);

        tokio::select! {
            _ = serving.as_mut() => {}
            _ = stalls.stalled(self.limits.stall_timeout) => emit::connection_stalled(),
            _ = self.cancel.cancelled() => {
                serving.as_mut().graceful_shutdown();
                let _ = serving.await;
            }
        }
    }
}
