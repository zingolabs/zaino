//! gRPC transport: hyper-util's HTTP/2 server with the routed tonic services mounted on it
//! (`server.bind().await?` at boot, then `tokio::spawn(bound.run(cancel))`)
//!
//! Serves the [`Router`], not the generated service directly. The router is what lets an
//! enabled index answer its own methods from stored bytes while everything else falls through
//! to [`GrpcService`] — one service name, two answer paths, chosen per method path.
//!
//! The stack around it, outermost first:
//!
//! ```text
//!   accept ─ connection caps ─ h2 conn ─ metrics ─ admission ─ Router
//! ```
//!
//! Serving hyper directly rather than `tonic::transport::Server` is what makes the caps
//! reachable: the accept loop is ours, so a connection over a cap is closed before a task
//! exists for it, and the h2 settings below are set per connection.

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
use tracing::{debug, warn, Instrument as _, Span};
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

use crate::admission::{Admission, Class, Permits};
use crate::client::TrustedProxies;
use crate::connections::{ConnectionCaps, Reserved};
use crate::grpc::GrpcService;
use crate::limits::ReadLanes;
use crate::observe::Measured;
use crate::report::{self, Held, INTERVAL};
use crate::validator::{ValidatorHandler, ValidatorPorts};
use crate::{emit, GrpcLimits, Router};

/// Per-stream send buffer. Flow control paces each stream from here, so a slow wallet backs
/// itself up rather than the server.
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

/// Ping cadence on an idle connection, and how long a pong may take.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Locally-reset streams a peer may accumulate before the connection is closed (CVE-2023-44487).
const RAPID_RESET_LIMIT: usize = 128;

/// The routed `CompactTxStreamer`: indexes first, node fallback behind them.
type Routed<S> = Router<CompactTxStreamerServer<GrpcService<S>>>;

/// A `CompactTxStreamer` server, bounded by [`GrpcLimits`].
pub struct GrpcServer<S> {
    routed: Routed<S>,
    bind: SocketAddr,
    limits: GrpcLimits,
    proxies: TrustedProxies,
}

/// Why the gRPC server could not run.
#[derive(Debug, thiserror::Error)]
pub enum GrpcServeError {
    /// The server failed to bind or its serve loop errored.
    #[error("gRPC server error: {0}")]
    Serve(String),
}

impl<S: ValidatorPorts> GrpcServer<S> {
    /// A server over `handler`, bounded by `limits`, with no index claiming anything yet
    ///
    /// - `handler` twice: the fallback service, and the bytes half of `GetTaddressTransactions`
    ///   (the router holds both halves of that method)
    pub fn new(handler: ValidatorHandler<S>, bind: SocketAddr, limits: GrpcLimits) -> Self {
        let raw = std::sync::Arc::new(handler.clone());
        let routed = Router::new(
            CompactTxStreamerServer::new(GrpcService::new(handler)),
            raw,
            ReadLanes::new(&limits),
        );

        Self { routed, bind, limits, proxies: TrustedProxies::default() }
    }

    /// Peers that front this server and name each client in a PROXY header (v1 or v2)
    ///
    /// - `max_connections_per_ip` then counts the named client, not the proxy
    /// - a trusted peer sending no header is dropped
    pub fn with_trusted_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.proxies = proxies;
        self
    }

    /// Lets the compact-block index answer the block methods.
    pub fn with_compact_block(
        mut self,
        service: zaino_index_compact_block::CompactBlockService,
    ) -> Self {
        self.routed = self.routed.with_compact_block(service);
        self
    }

    /// Lets the block-hash index resolve `BlockID.hash` for the other indexes' methods.
    pub fn with_block_hash(
        mut self,
        service: zaino_internal_block_hash_to_height::BlockHashService,
    ) -> Self {
        self.routed = self.routed.with_block_hash(service);
        self
    }

    /// Lets the tree-state index answer the treestate and subtree-root methods.
    pub fn with_tree_state(mut self, service: zaino_index_tree_state::TreeStateService) -> Self {
        self.routed = self.routed.with_tree_state(service);
        self
    }

    /// Lets the transparent-address index answer the utxo and balance methods.
    pub fn with_transparent_address(
        mut self,
        service: zaino_index_transparent_address::TransparentAddressService,
    ) -> Self {
        self.routed = self.routed.with_transparent_address(service);
        self
    }

    /// Lets the chain view answer the mempool methods and own the broadcast fan-out.
    pub fn with_chainview(mut self, handles: crate::ChainViewHandles) -> Self {
        self.routed = self.routed.with_chainview(handles);
        self
    }
}

/// The h2 settings every connection is served with.
fn http2(limits: &GrpcLimits) -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new()).http2_only();
    builder
        .http2()
        // Keepalive pings need a clock; hyper has none of its own.
        .timer(TokioTimer::new())
        .max_concurrent_streams(limits.max_streams_per_connection.get())
        .max_send_buf_size(SEND_BUFFER)
        .keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .max_local_error_reset_streams(RAPID_RESET_LIMIT);

    builder
}

/// [`GrpcServer`] holding its socket, ready to [`run`](Self::run)
pub struct BoundGrpcServer<S> {
    server: GrpcServer<S>,
    listener: TcpListener,
}

impl<S: ValidatorPorts> GrpcServer<S> {
    /// Awaited at boot, before spawning `run` (EADDRINUSE = boot failure, not a serve-loop
    /// failure (#1081))
    pub async fn bind(self) -> Result<BoundGrpcServer<S>, GrpcServeError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|e| GrpcServeError::Serve(format!("bind failed: {e}")))?;
        Ok(BoundGrpcServer { server: self, listener })
    }
}

/// The per-connection service stack (see the module doc)
type Stack<S> = TowerToHyperService<Measured<Admission<Routed<S>>>>;

/// What every connection task shares
struct Shared<S> {
    caps: ConnectionCaps,
    proxies: TrustedProxies,
    http2: auto::Builder<TokioExecutor>,
    routed: Routed<S>,
    permits: Permits,
    limits: GrpcLimits,
    cancel: CancellationToken,
}

impl<S> Shared<S> {
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

impl<S: ValidatorPorts> BoundGrpcServer<S> {
    /// Accept + serve until `cancel` (open connections then shut down gracefully)
    ///
    /// - never ends on an accept error: a resource error (fd limit, memory) backs off and
    ///   retries, the listener recovering as connections close
    pub async fn run(self, cancel: CancellationToken) -> Result<(), GrpcServeError> {
        let Self { server, listener } = self;
        let shared = Arc::new(Shared {
            caps: ConnectionCaps::new(&server.limits),
            proxies: server.proxies,
            http2: http2(&server.limits),
            routed: server.routed,
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
                _ = cancel.cancelled() => return Ok(()),
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
                        _ = cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                    continue;
                }
            };

            // Over a cap: the socket is dropped here, so the refusal costs no task.
            let Some(reserved) = shared.caps.reserve() else {
                continue;
            };
            if let Err(error) = tune(&socket) {
                debug!(%error, %peer, "Dropping a socket that refused its options");
                continue;
            }

            let connection = Arc::clone(&shared).connection(socket, peer, reserved);
            tokio::spawn(connection.instrument(Span::current()));
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

impl<S: ValidatorPorts> Shared<S> {
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
            return self.serve(socket).await;
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

        // Bytes read past the header belong to the client's HTTP/2 stream: replayed first
        let (reader, writer) = socket.into_split();
        let io = tokio::io::join(std::io::Cursor::new(read_ahead).chain(reader), writer);
        self.serve(io).await;
    }

    /// Until the peer closes, `cancel` (graceful), or a stream holds unread data past
    /// `stall_timeout` (dropped: graceful would wait on the very stream that stalled)
    async fn serve<I>(&self, io: I)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let stalls = Arc::new(crate::stall::Watch::default());
        let service: Stack<S> = TowerToHyperService::new(Measured::new(Admission::new(
            self.routed.clone(),
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
