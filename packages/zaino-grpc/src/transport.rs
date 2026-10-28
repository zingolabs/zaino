//! [`GrpcService`]: hyper-util's HTTP/2 server around the path dispatch (`service.rs`)
//! (`service.bind().await?` at boot, then `tokio::spawn(bound.run(cancel))`)
//!
//! - stack, outermost first: usage.md "Serve stack"
//! - hyper, not `tonic::transport::Server` (own accept loop: over-cap closed before a task
//!   exists, h2 settings per connection)

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
use zaino_persistence::PinnedView;
use zaino_source::ChainDataSource;

use crate::admission::AdmissionState;
use crate::connections::{ConnectionCaps, Reserved};
use crate::guard::Guard;
use crate::health::{Health, HealthRoute};
use crate::lifetime::{Activity, Retire, RetireReason, Retirement};
use crate::overload::Signals;
use crate::pressure::Pressure;
use crate::proxy_protocol;
use crate::service::{Dispatch, Routing};
use crate::summary::{self, INTERVAL};
use crate::{metrics, GrpcLimits};

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

/// HTTP/1.1 request header deadline (a health monitor sends its at once)
const HTTP1_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle-connection ping cadence + pong deadline
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Locally-reset streams per peer before the connection closes (CVE-2023-44487)
const RAPID_RESET_LIMIT: usize = 128;

/// `CompactTxStreamer` endpoint: enabled [`RouteDeps`] on one listener, bounded by [`GrpcLimits`]
pub struct GrpcService<S: ChainDataSource, V> {
    dispatch: Dispatch<S, V>,
    bind: SocketAddr,
    limits: GrpcLimits,
    tls: Option<crate::Tls>,
    admission: Arc<AdmissionState>,
    health: Option<Arc<dyn Health>>,
    retire: Retire,
}

/// Boot-time only: a bound server never fails ([`BoundGrpcService::run`] returns `()`)
#[derive(Debug, thiserror::Error)]
pub enum GrpcServeError {
    #[error("binding the gRPC listener: {0}")]
    Bind(#[source] std::io::Error),
}

impl<S: ChainDataSource, V: PinnedView> GrpcService<S, V> {
    /// Unrouted: health answered, every method `UNAVAILABLE` until [`Routing::route`]
    pub fn new(bind: SocketAddr, limits: GrpcLimits) -> Self {
        let dispatch = Dispatch::new(&limits);
        let admission = Arc::new(AdmissionState::new(&limits));
        let retire = Retire::default();
        Self { dispatch, bind, limits, tls: None, admission, health: None, retire }
    }

    /// Routes these methods once its owner has them, before or after `run`
    pub fn routing(&self) -> Routing<S, V> {
        self.dispatch.routing()
    }

    #[cfg(test)]
    pub(crate) fn with_routes(self, routes: crate::service::RouteDeps<S, V>) -> Self {
        self.routing().route(routes);
        self
    }

    /// The owner's operator hold: while held, every connection GOAWAY'd at the hold's deadline
    pub fn with_retire(mut self, retire: Retire) -> Self {
        self.retire = retire;
        self
    }

    /// Answers `GET /readyz` + `GET /status` on this listener, HTTP/1.1 or h2 (`health.rs`)
    pub fn with_health(mut self, health: Arc<dyn Health>) -> Self {
        self.health = Some(health);
        self
    }

    pub fn admission(&self) -> Arc<AdmissionState> {
        Arc::clone(&self.admission)
    }

    /// Terminates TLS on the listener (else plaintext h2c, for a TLS-terminating proxy in front)
    pub fn with_tls(mut self, tls: crate::Tls) -> Self {
        self.tls = Some(tls);
        self
    }
}

/// - h2 for gRPC
/// - HTTP/1.1 only ever reaches the health paths (`health.rs`)
fn http(limits: &GrpcLimits) -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        // header read timeout needs a clock (slowloris)
        .timer(TokioTimer::new())
        .header_read_timeout(HTTP1_HEADER_TIMEOUT);
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

impl<S: ChainDataSource, V: PinnedView> GrpcService<S, V> {
    /// Awaited at boot, before spawning `run` (EADDRINUSE = boot failure, not a serve-loop
    /// failure (#1081))
    pub async fn bind(self) -> Result<BoundGrpcService<S, V>, GrpcServeError> {
        let listener = TcpListener::bind(self.bind).await.map_err(GrpcServeError::Bind)?;
        Ok(BoundGrpcService { server: self, listener })
    }
}

/// Per-connection service stack (usage.md "Serve stack")
type Stack<S, V> = TowerToHyperService<HealthRoute<Guard<Dispatch<S, V>>>>;

/// What every connection task shares
struct Shared<S: ChainDataSource, V> {
    caps: ConnectionCaps,
    tls: Option<tokio_rustls::TlsAcceptor>,
    http: auto::Builder<TokioExecutor>,
    health: Option<Arc<dyn Health>>,
    retire: Retire,
    dispatch: Dispatch<S, V>,
    admission: Arc<AdmissionState>,
    limits: GrpcLimits,
    cancel: CancellationToken,
}

impl<S: ChainDataSource, V: PinnedView> BoundGrpcService<S, V> {
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept + serve until `cancel`, then drain: listener closed, every connection GOAWAY'd,
    /// returns once they all finish or `drain_timeout` passes (the rest dropped)
    ///
    /// - never ends on an accept error: a resource error (fd limit, memory) backs off and
    ///   retries, the listener recovering as connections close
    pub async fn run(self, cancel: CancellationToken) {
        let drain_timeout = self.server.limits.drain_timeout;
        let connections = TaskTracker::new();
        self.accept(&cancel, &connections).await;
        connections.close();
        let open = connections.len();
        if open == 0 || drain_timeout.is_zero() {
            return;
        }
        info!(open, timeout = ?drain_timeout, "Draining connections");
        if tokio::time::timeout(drain_timeout, connections.wait()).await.is_err() {
            warn!(open = connections.len(), timeout = ?drain_timeout, "Drain timed out, dropping");
        }
    }

    /// Until `cancel`, each connection on `connections` (return drops the listener)
    async fn accept(self, cancel: &CancellationToken, connections: &TaskTracker) {
        let Self { server, listener } = self;
        if let Some(tls) = &server.tls {
            tokio::spawn(tls.reload(cancel.clone()).instrument(Span::current()));
        }
        let (mut cpu, mut io) = (Pressure::new("cpu"), Pressure::new("io"));
        let shared = Arc::new(Shared {
            caps: ConnectionCaps::new(&server.limits),
            tls: server.tls.as_ref().map(crate::Tls::acceptor),
            http: http(&server.limits),
            health: server.health,
            retire: server.retire,
            dispatch: server.dispatch,
            admission: server.admission,
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
                    let elapsed = now.saturating_duration_since(window_opened);
                    let signals = Signals { cpu: cpu.share(elapsed), io: io.share(elapsed) };
                    let at_capacity = shared.admission.tick(elapsed, signals);
                    summary::summarise(elapsed, shared.caps.held(), at_capacity);
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
                    metrics::accept_failed();
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

            let retirement = shared.retire.subscribe();
            let connection = Arc::clone(&shared).connection(socket, peer, reserved, retirement);
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

impl<S: ChainDataSource, V: PinnedView> Shared<S, V> {
    /// Names the client, applies its cap, then serves HTTP/2 until close or `cancel`
    async fn connection(
        self: Arc<Self>,
        mut socket: TcpStream,
        peer: SocketAddr,
        reserved: Reserved,
        retirement: Retirement,
    ) {
        let peer_ip = peer.ip().to_canonical();
        if !self.limits.trusted_proxies.trusts(peer_ip) {
            let Some(_connection) = self.caps.admit(reserved, peer_ip) else {
                return;
            };
            return self.secure(socket, peer, retirement).await;
        }

        let header = tokio::time::timeout(
            proxy_protocol::HEADER_TIMEOUT,
            proxy_protocol::read_header(&mut socket),
        )
        .await
        .unwrap_or(Err(proxy_protocol::HeaderError::Timeout));
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
        self.secure(io, peer, retirement).await;
    }

    /// TLS handshake when configured (bounded by [`TLS_HANDSHAKE_TIMEOUT`]), then [`Self::serve`]
    async fn secure<I>(&self, io: I, peer: SocketAddr, retirement: Retirement)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let Some(acceptor) = &self.tls else {
            return self.serve(io, retirement).await;
        };
        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(io)).await {
            Ok(Ok(stream)) => self.serve(stream, retirement).await,
            // scanners and plaintext clients: routine, not an operator signal
            Ok(Err(error)) => debug!(%error, %peer, "TLS handshake failed"),
            Err(_) => debug!(%peer, "TLS handshake timed out"),
        }
    }

    /// Until the peer closes, `cancel` (graceful), or a stream holds unread data past
    /// `unread_timeout` (dropped: graceful would wait on the very stream that stalled)
    ///
    /// - aged or idle (`lifetime.rs`): GOAWAY, in-flight streams get `max_connection_age_grace`
    async fn serve<I>(&self, io: I, retirement: Retirement)
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let stalls = Arc::new(crate::stall::Watch::default());
        let activity = Arc::new(Activity::default());
        let admission = Arc::clone(&self.admission);
        let guard =
            Guard::new(self.dispatch.clone(), admission, Arc::clone(&stalls), activity.clone());
        let service: Stack<S, V> =
            TowerToHyperService::new(HealthRoute::new(guard, self.health.clone()));
        let serving = self.http.serve_connection(TokioIo::new(io), service);
        let mut serving = std::pin::pin!(serving);
        let limits = &self.limits;
        let aged = async {
            match limits.max_connection_age {
                Some(age) => tokio::time::sleep(crate::lifetime::jittered(age)).await,
                None => std::future::pending().await,
            }
        };
        let idled = async {
            match limits.max_connection_idle {
                Some(idle) => activity.idle(idle).await,
                None => std::future::pending().await,
            }
        };
        let retired = tokio::select! {
            _ = serving.as_mut() => return,
            _ = stalls.stalled(limits.unread_timeout) => return metrics::connection_stalled(),
            _ = self.cancel.cancelled() => {
                serving.as_mut().graceful_shutdown();
                let _ = serving.await;
                return;
            }
            () = aged => RetireReason::Age,
            () = idled => RetireReason::Idle,
            () = retirement.fired() => RetireReason::Hold,
        };
        metrics::connection_retired(retired);
        serving.as_mut().graceful_shutdown();
        tokio::select! {
            _ = tokio::time::timeout(limits.max_connection_age_grace, serving.as_mut()) => {}
            _ = stalls.stalled(limits.unread_timeout) => metrics::connection_stalled(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_util::sync::CancellationToken;
    use tonic::Code;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_persistence::DiskView;
    use zaino_proto::proto::service;
    use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
    use zaino_source::testing::{p2pkh, MockChain};
    use zaino_source::ZcashLegacyRpcClient;
    use zaino_sync::IndexKind;

    use super::{GrpcServeError, GrpcService};
    use crate::service::{Method, RouteDeps};
    use crate::testing::{fixed_snapshots, indexed, routes_over, routes_serving, MAINNET};
    use crate::{GrpcLimits, TrustedProxies};

    const ANY_PORT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    /// Real-clock headroom past a deadline (loaded runner: wake-ups + loopback handshakes)
    const SCHEDULING: Duration = Duration::from_secs(1);

    /// Nothing served, chain view never polled: every `GetLightdInfo` = `UNAVAILABLE`
    fn unserved() -> RouteDeps<ZcashLegacyRpcClient, DiskView> {
        let chain = MockChain::regtest();
        routes_over(&chain.validator(chain.genesis()), MAINNET, &[]).0
    }

    const NOTHING_SERVED: &str = "the indexes are syncing: nothing served yet";

    /// A wallet whose TCP connection opens with a PROXY v1 header naming `client`
    async fn through_proxy(
        bind: SocketAddr,
        client: &str,
    ) -> CompactTxStreamerClient<tonic::transport::Channel> {
        let header = format!("PROXY TCP4 {client} 127.0.0.1 40000 8137\r\n");
        let channel = tonic::transport::Endpoint::from_static("http://proxied")
            .connect_with_connector(tower::service_fn(move |_| {
                let header = header.clone();
                async move {
                    let mut stream = tokio::net::TcpStream::connect(bind).await?;
                    stream.write_all(header.as_bytes()).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .expect("the proxy connects");
        CompactTxStreamerClient::new(channel)
    }

    /// Bytes `stream` sends before it closes (`Err` = still open after `SCHEDULING`)
    async fn read_until_closed(stream: &mut tokio::net::TcpStream) -> Result<usize, &'static str> {
        let mut read = [0u8; 1];
        match tokio::time::timeout(SCHEDULING, stream.read(&mut read)).await {
            Ok(read) => Ok(read.expect("read")),
            Err(_) => Err("held open, never closed"),
        }
    }

    #[tokio::test]
    async fn a_wallet_is_served_and_a_second_connection_from_its_address_is_closed_at_accept() {
        let one = NonZeroUsize::MIN;
        let limits = GrpcLimits { max_connections_per_ip: one, ..GrpcLimits::default() };
        let server = GrpcService::new(ANY_PORT, limits).with_routes(unserved());
        let bound = server.bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        let cancel = CancellationToken::new();
        let serving = tokio::spawn(bound.run(cancel.clone()));

        let mut wallet =
            CompactTxStreamerClient::connect(format!("http://{bind}")).await.expect("connects");
        let refusal = wallet.get_lightd_info(service::Empty {}).await.expect_err("nothing served");
        assert_eq!(
            (refusal.code(), refusal.message()),
            (Code::Unavailable, NOTHING_SERVED),
            "decoded over h2"
        );

        let mut second =
            tokio::net::TcpStream::connect(bind).await.expect("kernel completes the handshake");
        let sent = read_until_closed(&mut second).await;
        assert_eq!(sent, Ok(0), "second connection from 127.0.0.1 over max_connections_per_ip 1");

        cancel.cancel();
        serving.await.expect("the serve task ran");
    }

    /// Behind a trusted proxy the per-address cap counts the PROXY header's client, never the
    /// proxy's address
    #[tokio::test]
    async fn behind_a_trusted_proxy_the_per_address_cap_counts_the_named_client() {
        let loopback = vec!["127.0.0.0/8".parse().expect("loopback net")];
        let limits = GrpcLimits {
            max_connections_per_ip: NonZeroUsize::MIN,
            trusted_proxies: TrustedProxies::new(loopback),
            ..GrpcLimits::default()
        };
        let server = GrpcService::new(ANY_PORT, limits).with_routes(unserved());
        let bound = server.bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        let cancel = CancellationToken::new();
        let serving = tokio::spawn(bound.run(cancel.clone()));
        let empty = service::Empty {};

        let mut first = through_proxy(bind, "203.0.113.1").await;
        let mut second = through_proxy(bind, "203.0.113.2").await;
        for (client, wallet) in [("203.0.113.1", &mut first), ("203.0.113.2", &mut second)] {
            let answered = wallet.get_lightd_info(empty).await.expect_err("nothing served");
            assert_eq!(
                answered.code(),
                Code::Unavailable,
                "{client}: served under its own cap: {answered:?}"
            );
        }

        let mut repeat = through_proxy(bind, "203.0.113.1").await;
        let closed = repeat.get_lightd_info(empty).await.expect_err("203.0.113.1 already held");
        assert_eq!(closed.code(), Code::Unknown, "closed at accept, never answered: {closed:?}");

        let mut headerless = tokio::net::TcpStream::connect(bind).await.expect("accepting");
        headerless.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.expect("preface");
        let sent = read_until_closed(&mut headerless).await;
        assert_eq!(sent, Ok(0), "trusted peer speaking h2 with no PROXY header: closed unserved");

        cancel.cancel();
        serving.await.expect("the serve task ran");
    }

    /// Cancel closes the listener at once, then `run` drains: an idle wallet closes on its GOAWAY,
    /// a never-finishing connection (trusted peer silent in its PROXY read) holds `run` exactly
    /// `drain_timeout`
    #[tokio::test]
    async fn cancel_closes_the_listener_then_waits_for_open_connections_at_most_the_drain_timeout()
    {
        // case, a silent connection open, drain_timeout, run returns no sooner than
        #[rustfmt::skip]
        let cases = [
            ("idle wallet only", false, Duration::from_secs(30), Duration::ZERO),
            ("silent peer",      true,  Duration::from_secs(1),  Duration::from_secs(1)),
        ];
        for (case, silent, drain_timeout, earliest) in cases {
            let loopback = vec!["127.0.0.0/8".parse().expect("loopback net")];
            let limits = GrpcLimits {
                drain_timeout,
                trusted_proxies: TrustedProxies::new(loopback),
                ..GrpcLimits::default()
            };
            let server = GrpcService::new(ANY_PORT, limits).with_routes(unserved());
            let bound = server.bind().await.expect("binds");
            let bind = bound.local_addr().expect("bound");
            let cancel = CancellationToken::new();
            let serving = tokio::spawn(bound.run(cancel.clone()));
            // before the wallet (accept FIFO: the wallet's answer proves this one accepted)
            let _silent = match silent {
                true => Some(tokio::net::TcpStream::connect(bind).await.expect("accepting")),
                false => None,
            };
            let mut wallet = through_proxy(bind, "203.0.113.1").await;
            let answered =
                wallet.get_lightd_info(service::Empty {}).await.expect_err("nothing served");
            assert_eq!(answered.code(), Code::Unavailable, "{case}: served first: {answered:?}");

            let cancelled = std::time::Instant::now();
            cancel.cancel();
            serving.await.expect("the serve task ran");
            let took = cancelled.elapsed();

            let window = earliest..earliest + SCHEDULING;
            assert!(
                window.contains(&took),
                "{case}: run returned after {took:?}, not in {window:?}"
            );
            let refused =
                tokio::net::TcpStream::connect(bind).await.map(drop).map_err(|e| e.kind());
            assert_eq!(
                refused,
                Err(std::io::ErrorKind::ConnectionRefused),
                "{case}: listener closed"
            );
            let after =
                wallet.get_lightd_info(service::Empty {}).await.expect_err("connection closed");
            let reconnect_refused = after.message().contains("Connection refused");
            assert!(
                reconnect_refused,
                "{case}: old connection closed, reconnect refused: {after:?}"
            );
        }
    }

    /// #1081: EADDRINUSE = `bind()`'s error, never a spawned serve loop's
    #[tokio::test]
    async fn binding_a_held_port_fails_before_anything_is_served() {
        let held = tokio::net::TcpListener::bind(ANY_PORT).await.expect("a free port");
        let bind = held.local_addr().expect("local addr");

        let outcome =
            GrpcService::new(bind, GrpcLimits::default()).with_routes(unserved()).bind().await;

        let error = outcome.err();
        let kind = error.as_ref().map(|GrpcServeError::Bind(error)| error.kind());
        assert_eq!(kind, Some(std::io::ErrorKind::AddrInUse), "{error:?}");
    }

    /// Wallet on an h2 >= 0.4.16 client (charges every DATA frame < 256 B against a
    /// connection budget until read: `GOAWAY too_many_data_frames`, lightwalletd #593), reading
    /// only after the server filled its window, over the real transport:
    /// - 10,000 UTXOs of one address (~100 B each) stream whole, height order
    /// - 10,000 blocks (~90 B each, shielded-only) stream whole, then `OK`
    #[tokio::test]
    async fn a_slow_h2_reader_gets_ten_thousand_small_messages_without_a_goaway() {
        const COUNT: u32 = 10_000;
        // `t1Hsc…` = hash160 `00…00`
        const ALICE: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        let alice = p2pkh([0x00; 20]);
        let mut chain = MockChain::regtest()
            .network(MAINNET)
            .genesis_with(|b| b.coinbase(|c| c.pay(&alice, 1_000)));
        for _ in 1..COUNT {
            chain.mine(|b| b.coinbase(|c| c.pay(&alice, 1_000)));
        }
        let tip = chain.tip();
        let indexes = [IndexKind::CompactBlock, IndexKind::TransparentAddress];
        let views = indexes.map(|kind| indexed(kind, &chain, tip)).to_vec();
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let routes = routes_serving(MAINNET, &indexes, snapshots);

        let server = GrpcService::new(ANY_PORT, GrpcLimits::default()).with_routes(routes);
        let bound = server.bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(bound.run(cancel.clone()));
        // h2's own default windows (tonic's are 5 MiB / 2 MiB): budget 32 KiB ≈ 200 unread frames
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{bind}"))
            .expect("a URI")
            .initial_connection_window_size(65_535)
            .initial_stream_window_size(65_535)
            .connect()
            .await
            .expect("connects");
        let mut wallet = CompactTxStreamerClient::new(channel);

        let request = service::GetAddressUtxosArg {
            addresses: vec![ALICE.to_owned()],
            start_height: 0,
            max_entries: 0,
        };
        let mut utxos = wallet.get_address_utxos_stream(request).await.expect("opens").into_inner();
        // unread while the server fills the 64 KiB window (the frames h2 >= 0.4.16 charges)
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut heights = Vec::new();
        while let Some(utxo) = utxos.message().await.expect("no GOAWAY mid-stream") {
            heights.push(utxo.height);
        }
        assert_eq!(heights, (0..u64::from(COUNT)).collect::<Vec<_>>(), "every UTXO, in order");

        let at = |height| Some(service::BlockId { height, hash: Vec::new() });
        let range = service::BlockRange { start: at(0), end: at(9_999), pool_types: Vec::new() };
        let mut range = wallet.get_block_range(range).await.expect("opens").into_inner();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut heights = Vec::new();
        while let Some(block) = range.message().await.expect("no GOAWAY mid-stream") {
            heights.push(block.height);
        }
        assert_eq!(heights, (0..u64::from(COUNT)).collect::<Vec<_>>(), "every block, in order");
        cancel.cancel();
    }

    /// Lifetime test server: `limits` over a transparent-address index at genesis
    async fn serving_lifetimes(limits: GrpcLimits) -> SocketAddr {
        let chain = MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, genesis)];
        let snapshots =
            fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
        let routes = routes_serving(MAINNET, &[IndexKind::TransparentAddress], snapshots);
        let bound =
            GrpcService::new(ANY_PORT, limits).with_routes(routes).bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        tokio::spawn(bound.run(CancellationToken::new()));
        bind
    }

    type Lifetime = tokio::task::JoinHandle<(Result<(), h2::Error>, Duration)>;

    /// Raw h2 client (sees the GOAWAY a tonic channel hides) + its connection's lifetime, timed
    /// from before the connect (the server's clocks start at accept, before the handshake)
    async fn h2_client(bind: SocketAddr) -> (h2::client::SendRequest<bytes::Bytes>, Lifetime) {
        let opened = std::time::Instant::now();
        let tcp = tokio::net::TcpStream::connect(bind).await.expect("connects");
        let (client, connection) = h2::client::handshake(tcp).await.expect("h2 handshake");
        let lifetime = tokio::spawn(async move { (connection.await, opened.elapsed()) });
        (client.ready().await.expect("ready"), lifetime)
    }

    /// `GetTaddressBalanceStream` opened, alice's address sent, request left open
    fn open_balance_stream(
        client: &mut h2::client::SendRequest<bytes::Bytes>,
    ) -> Result<(h2::client::ResponseFuture, h2::SendStream<bytes::Bytes>), h2::Error> {
        use prost::Message as _;

        let request =
            http::Request::post(format!("http://zaino{}", Method::GetTaddressBalanceStream.path()))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .body(())
                .expect("request");
        let (response, mut sending) = client.send_request(request, false)?;
        let alice = service::Address { address: "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned() };
        let mut framed = Vec::new();
        zaino_proto::frame::frame_into(&mut framed, |out| alice.encode_raw(out));
        sending.send_data(framed.into(), false).expect("first message");
        Ok((response, sending))
    }

    /// gRPC A9: in flight at the age = GOAWAY + `max_connection_age_grace` to finish
    #[tokio::test]
    async fn an_aged_connection_refuses_new_streams_and_still_answers_the_one_in_flight() {
        let age = Duration::from_secs(1);
        let grace = Duration::from_secs(5);
        let limits = GrpcLimits {
            max_connection_age: Some(age),
            max_connection_age_grace: grace,
            max_connection_idle: None,
            ..GrpcLimits::default()
        };
        let bind = serving_lifetimes(limits).await;
        let (mut client, lifetime) = h2_client(bind).await;
        let (response, mut request) = open_balance_stream(&mut client).expect("opens");

        let oldest = age + age / crate::lifetime::JITTER_DIVISOR;
        tokio::time::sleep(oldest + SCHEDULING).await;

        let refused = match client.ready().await {
            Ok(mut ready) => open_balance_stream(&mut ready).map(drop),
            Err(error) => Err(error),
        };
        let refused = refused.expect_err("past the age: no new stream");
        assert!(refused.is_go_away(), "refused by the GOAWAY: {refused:?}");
        request.send_data(bytes::Bytes::new(), true).expect("request ended");
        let mut body = response.await.expect("in-flight stream answered").into_body();
        let balance = body.data().await.expect("one message").expect("readable");
        let trailers = body.trailers().await.expect("readable").expect("unary: status trails");
        let status = tonic::Status::from_header_map(&trailers).expect("grpc-status");
        assert_eq!(status.code(), Code::Ok, "in flight past the age, in its grace: {status:?}");
        assert_ne!(balance.len(), 0, "the balance message");
        let (closed, lived) =
            tokio::time::timeout(grace, lifetime).await.expect("closed").expect("joins");
        assert!(
            lived < oldest + SCHEDULING * 2,
            "closed once the stream ended, not at the grace: {lived:?} ({closed:?})"
        );
    }

    #[tokio::test]
    async fn a_stream_held_past_the_age_grace_is_reset_with_its_connection() {
        let (age, grace) = (Duration::from_secs(1), Duration::from_secs(1));
        let limits = GrpcLimits {
            max_connection_age: Some(age),
            max_connection_age_grace: grace,
            max_connection_idle: None,
            ..GrpcLimits::default()
        };
        let bind = serving_lifetimes(limits).await;
        let (mut client, lifetime) = h2_client(bind).await;
        let (response, _held_open) = open_balance_stream(&mut client).expect("opens");

        let (closed, lived) = lifetime.await.expect("joins");

        let jitter = age / crate::lifetime::JITTER_DIVISOR;
        let window = age - jitter + grace..age + jitter + grace + SCHEDULING;
        assert!(
            window.contains(&lived),
            "closed after {lived:?}, not age ± 10% + grace {window:?} ({closed:?})"
        );
        let reset = response.await.expect_err("held stream outlived its connection");
        assert!(reset.is_io(), "connection dropped under it, no RST_STREAM: {reset:?}");
    }

    #[tokio::test]
    async fn an_unused_connection_closes_after_max_connection_idle() {
        let idle = Duration::from_secs(1);
        let limits = GrpcLimits {
            max_connection_age: None,
            max_connection_idle: Some(idle),
            ..GrpcLimits::default()
        };
        let bind = serving_lifetimes(limits).await;

        let (_client, lifetime) = h2_client(bind).await;

        let (closed, lived) = lifetime.await.expect("joins");
        let window = idle..idle + SCHEDULING;
        assert!(window.contains(&lived), "closed after {lived:?}, not in {window:?} ({closed:?})");
    }

    /// One TLS listener as a load balancer's HTTPS/1.1 monitor and a wallet each see it (health
    /// routing itself: `health.rs`); bound before routed = an owner still booting
    #[tokio::test]
    async fn one_tls_listener_answers_health_from_bind_and_grpc_once_routed() {
        use std::sync::Arc;

        use bytes::Bytes;
        use rustls::pki_types::ServerName;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio_rustls::TlsConnector;

        struct Serving;
        impl crate::Health for Serving {
            fn readyz(&self) -> (bool, String) {
                (true, r#"{"state":"serving","detail":null}"#.to_owned())
            }
            fn status(&self) -> String {
                r#"{"state":"serving"}"#.to_owned()
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let files = crate::TlsFiles {
            cert_path: dir.path().join("cert.pem"),
            key_path: dir.path().join("key.pem"),
        };
        let host = "mainnet.zainod.zingolabs.dev";
        let cert = rcgen::generate_simple_self_signed(vec![host.to_owned()]).expect("cert");
        std::fs::write(&files.cert_path, cert.cert.pem()).expect("cert pem");
        std::fs::write(&files.key_path, cert.signing_key.serialize_pem()).expect("key pem");
        let server = GrpcService::new(ANY_PORT, crate::GrpcLimits::default())
            .with_tls(crate::Tls::load(files).expect("pair loads"))
            .with_health(Arc::new(Serving));
        let (admission, routing) = (server.admission(), server.routing());
        let bound = server.bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        tokio::spawn(bound.run(tokio_util::sync::CancellationToken::new()));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).expect("root");
        let connector = |alpn: &[u8]| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut client = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots.clone())
                .with_no_client_auth();
            client.alpn_protocols = vec![alpn.to_vec()];
            TlsConnector::from(Arc::new(client))
        };
        let tls = |alpn: &'static [u8]| {
            let connector = connector(alpn);
            async move {
                let tcp = tokio::net::TcpStream::connect(bind).await.expect("connects");
                let name = ServerName::try_from(host).expect("name");
                let stream = connector.connect(name, tcp).await.expect("handshake");
                assert_eq!(stream.get_ref().1.alpn_protocol(), Some(alpn), "ALPN");
                stream
            }
        };
        let readyz = || async move {
            let mut stream = tls(b"http/1.1").await;
            let request =
                format!("GET /readyz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.expect("request");
            let mut response = String::new();
            stream.read_to_string(&mut response).await.expect("response");
            let status = response.split(' ').nth(1).unwrap_or_default().to_owned();
            let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
            (status, body)
        };
        // one `GetLightdInfo` over its own h2 connection
        let lightd_info = || async move {
            let (client, connection) = h2::client::handshake(tls(b"h2").await).await.expect("h2");
            tokio::spawn(connection);
            let mut client = client.ready().await.expect("ready");
            let path = Method::GetLightdInfo.path();
            let request = http::Request::post(format!("https://{host}{path}"))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .body(())
                .expect("request");
            let (response, mut body) = client.send_request(request, false).expect("sent");
            body.send_data(Bytes::from_static(b"\0\0\0\0\0"), true).expect("empty Empty");
            let response = response.await.expect("answered");
            let status = tonic::Status::from_header_map(response.headers()).expect("Trailers-Only");
            (status.code(), status.message().to_owned())
        };

        let healthy = ("200".to_owned(), r#"{"state":"serving","detail":null}"#.to_owned());
        assert_eq!(readyz().await, healthy, "ALPN http/1.1 /readyz, unrouted");
        assert_eq!(
            admission.held(crate::admission::Class::Work),
            0,
            "a monitor's /readyz never holds a work slot"
        );
        let starting = (Code::Unavailable, crate::service::STARTING.to_owned());
        assert_eq!(lightd_info().await, starting, "ALPN h2 → the gRPC stack, unrouted");

        routing.route(unserved());
        let routed = (Code::Unavailable, NOTHING_SERVED.to_owned());
        assert_eq!(lightd_info().await, routed, "routed after bind: the routes' own answer");
    }
}
