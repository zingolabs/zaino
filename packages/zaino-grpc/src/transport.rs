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
use crate::lifetime::{Activity, Retire, Retirement};
use crate::network;
use crate::overload::Signals;
use crate::pressure::Pressure;
use crate::proxy_protocol::{self, TrustedProxies};
use crate::service::{Dispatch, RouteDeps, Routing};
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

    /// Routed from the start
    pub fn with_routes(self, routes: RouteDeps<S, V>) -> Self {
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

    /// Admission pools, read by `report()`
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
    proxies: TrustedProxies,
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
    /// The bound address (port `0` at bind = the one the kernel picked)
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
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
        let network = Arc::new(network::Window::default());
        if let Ok(bound) = listener.local_addr() {
            let sampling = network::run(bound.port(), Arc::clone(&network), cancel.clone());
            tokio::spawn(sampling.instrument(Span::current()));
        }
        let (mut cpu, mut io) = (Pressure::new("cpu"), Pressure::new("io"));
        let shared = Arc::new(Shared {
            caps: ConnectionCaps::new(&server.limits),
            proxies: server.limits.trusted_proxies.clone(),
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
                    let elapsed = now - window_opened;
                    let min_networks = shared.admission.overload_limits().network_min_networks;
                    let saturation = network.verdict(min_networks.get());
                    let signals = Signals {
                        network_delay: saturation.delay,
                        network_loss: saturation.loss,
                        cpu: cpu.share(elapsed),
                        io: io.share(elapsed),
                    };
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
        if !self.proxies.trusts(peer_ip) {
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
            () = aged => "age",
            () = idled => "idle",
            () = retirement.fired() => "retired",
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
    use std::time::Duration;

    use zaino_persistence::IndexKind;
    use zaino_proto::proto::service;
    use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
    use zaino_source::testing::{p2pkh, MockChain};

    use super::GrpcService;
    use crate::service::{Method, RouteDeps};
    use crate::testing::{grpc_snapshots, indexed, routes, MAINNET};

    const ANY_PORT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

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
        let views = vec![
            indexed(IndexKind::CompactBlock, &chain, tip),
            indexed(IndexKind::TransparentAddress, &chain, tip),
        ];
        let routes = RouteDeps { grpc_snapshots: grpc_snapshots(&chain, tip, views), ..routes() };

        let server = GrpcService::new(ANY_PORT, crate::GrpcLimits::default()).with_routes(routes);
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

    /// gRPC A9 over a real socket, raw h2 client (sees the GOAWAY a tonic channel hides):
    ///
    /// - aged (1 s ±10%) with a `GetTaddressBalanceStream` in flight: GOAWAY (a new stream
    ///   refused), the in-flight one still answered `OK` once its client ends it, then closed
    /// - aged with a stream held past the grace (1 s): closed at age + grace, the stream reset
    /// - no age, idle 1 s, never used: closed at ~1 s
    /// - no age or idle, held (delay 0): open connection + one opened while held close at once
    ///   (nothing in flight)
    /// - after `resume`: new connection stays up
    #[tokio::test(flavor = "multi_thread")]
    async fn connections_retire_at_their_age_or_idleness_and_in_flight_streams_get_the_grace() {
        use std::time::Instant;

        use bytes::Bytes;
        use prost::Message as _;

        let chain = MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let serve = |limits: crate::GrpcLimits| {
            let views = vec![indexed(IndexKind::TransparentAddress, &chain, genesis)];
            let routes =
                RouteDeps { grpc_snapshots: grpc_snapshots(&chain, genesis, views), ..routes() };
            async move {
                let server = GrpcService::new(ANY_PORT, limits).with_routes(routes);
                let bound = server.bind().await.expect("binds");
                let bind = bound.local_addr().expect("bound");
                tokio::spawn(bound.run(tokio_util::sync::CancellationToken::new()));
                bind
            }
        };
        // timed from before the connect (the server's clocks start at accept, before the handshake)
        let connect = |bind| async move {
            let opened = Instant::now();
            let tcp = tokio::net::TcpStream::connect(bind).await.expect("connects");
            let (client, connection) = h2::client::handshake(tcp).await.expect("h2 handshake");
            let closed = tokio::spawn(async move { (connection.await, opened.elapsed()) });
            (client, closed)
        };
        let balance_stream = |client: &mut h2::client::SendRequest<Bytes>, bind| {
            let path = Method::GetTaddressBalanceStream.path();
            let request = http::Request::post(format!("http://{bind}{path}"))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .body(())
                .expect("request");
            client.send_request(request, false)
        };
        let alice = service::Address { address: "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned() };
        let mut framed = Vec::new();
        zaino_proto::frame::frame_into(&mut framed, |out| alice.encode_raw(out));
        let second = Duration::from_secs(1);

        let aged = crate::GrpcLimits {
            max_connection_age: Some(second),
            max_connection_age_grace: Duration::from_secs(5),
            max_connection_idle: None,
            ..crate::GrpcLimits::default()
        };
        let bind = serve(aged.clone()).await;
        let (client, closed) = connect(bind).await;
        let mut client = client.ready().await.expect("ready");
        let (response, mut request) = balance_stream(&mut client, bind).expect("opens");
        request.send_data(Bytes::from(framed.clone()), false).expect("first message");
        tokio::time::sleep(second.mul_f64(1.5)).await;
        let refused =
            client.ready().await.and_then(|mut ready| balance_stream(&mut ready, bind).map(drop));
        assert!(refused.is_err(), "past the age: GOAWAY, no new stream");
        assert!(!closed.is_finished(), "the in-flight stream holds the connection open");
        request.send_data(Bytes::new(), true).expect("request ended");
        let response = response.await.expect("answered");
        let mut body = response.into_body();
        let balance = body.data().await.expect("one message").expect("readable");
        assert!(!balance.is_empty(), "the balance");
        let trailers = body.trailers().await.expect("trailers").expect("unary: status trails");
        let status = trailers.get("grpc-status").map(|value| value.as_bytes());
        assert_eq!(status, Some(&b"0"[..]), "answered OK");
        let (_, lived) =
            tokio::time::timeout(second, closed).await.expect("closed").expect("joins");
        assert!(lived < Duration::from_secs(3), "closed once the stream ended: {lived:?}");

        let short_grace = crate::GrpcLimits { max_connection_age_grace: second, ..aged };
        let bind = serve(short_grace).await;
        let (client, closed) = connect(bind).await;
        let mut client = client.ready().await.expect("ready");
        let (response, mut request) = balance_stream(&mut client, bind).expect("opens");
        request.send_data(Bytes::from(framed.clone()), false).expect("first message");
        let (_, lived) = tokio::time::timeout(Duration::from_secs(5), closed)
            .await
            .expect("closed at age + grace")
            .expect("joins");
        let window = Duration::from_millis(1_800)..Duration::from_millis(3_000);
        assert!(window.contains(&lived), "age (0.9–1.1 s) + grace (1 s): {lived:?}");
        assert!(response.await.is_err(), "the held stream reset with its connection");

        let idle = crate::GrpcLimits {
            max_connection_age: None,
            max_connection_idle: Some(second),
            ..crate::GrpcLimits::default()
        };
        let bind = serve(idle).await;
        let (_client, closed) = connect(bind).await;
        let (_, lived) = tokio::time::timeout(Duration::from_secs(5), closed)
            .await
            .expect("closed when idle")
            .expect("joins");
        let window = second..Duration::from_millis(2_000);
        assert!(window.contains(&lived), "idle 1 s: {lived:?}");

        let forever = crate::GrpcLimits {
            max_connection_age: None,
            max_connection_idle: None,
            ..crate::GrpcLimits::default()
        };
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, genesis)];
        let routes =
            RouteDeps { grpc_snapshots: grpc_snapshots(&chain, genesis, views), ..routes() };
        let retire = crate::Retire::default();
        let server = GrpcService::new(ANY_PORT, forever).with_routes(routes);
        let bound = server.with_retire(retire.clone()).bind().await.expect("binds");
        let bind = bound.local_addr().expect("bound");
        tokio::spawn(bound.run(tokio_util::sync::CancellationToken::new()));
        let (client, open) = connect(bind).await;
        let mut client = client.ready().await.expect("ready");
        let (response, mut request) = balance_stream(&mut client, bind).expect("opens");
        request.send_data(Bytes::from(framed), true).expect("one message, ended");
        response.await.expect("served before the hold");
        retire.hold(Duration::ZERO);
        let (_, lived) = tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .expect("retired")
            .expect("joins");
        assert!(lived < Duration::from_secs(2), "GOAWAY'd at once: {lived:?}");
        let (_held_client, during) = connect(bind).await;
        let (_, lived) = tokio::time::timeout(Duration::from_secs(2), during)
            .await
            .expect("opened while held: retired too")
            .expect("joins");
        assert!(lived < Duration::from_secs(2), "GOAWAY'd at once: {lived:?}");
        retire.resume();
        let (client, later) = connect(bind).await;
        let _ready = client.ready().await.expect("a new connection is served");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!later.is_finished(), "opened after the resume: untouched");
    }

    /// One TLS listener, as a load balancer's HTTPS/1.1 monitor and a wallet each see it:
    ///
    /// - bound before routed (an owner still booting): health answered, every method
    ///   `UNAVAILABLE` [`crate::service::STARTING`]
    /// - routed later = the routes' own answer
    /// - ALPN `http/1.1`: `GET /readyz` = 200 + the health body, `/status` = 200, a gRPC path = 404
    /// - ALPN `h2`: `GetLightdInfo` reaches the gRPC stack (`UNAVAILABLE`: nothing served)
    /// - a monitor's `/readyz` never takes a work slot (`in_flight` stays 0)
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
        let get = |target: String| async move {
            let mut stream = tls(b"http/1.1").await;
            let request =
                format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.expect("request");
            let mut response = String::new();
            stream.read_to_string(&mut response).await.expect("response");
            let status = response.split(' ').nth(1).unwrap_or_default().to_owned();
            let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
            (status, body)
        };

        // (grpc-status, grpc-message) of one `GetLightdInfo` over its own h2 connection
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
            let header = |name| {
                let value = response.headers().get(name).map(|value| value.to_str());
                value.and_then(Result::ok).unwrap_or_default().to_owned()
            };
            (header("grpc-status"), header("grpc-message"))
        };

        let readyz = get("/readyz".to_owned()).await;
        let healthy = ("200".to_owned(), r#"{"state":"serving","detail":null}"#.to_owned());
        assert_eq!(readyz, healthy, "monitor's /readyz, unrouted");
        assert_eq!(get("/status".to_owned()).await.0, "200", "/status, unrouted");
        assert_eq!(get(Method::GetLightdInfo.path()).await.0, "404", "gRPC needs h2");
        assert_eq!(admission.report().in_flight, 0, "health never holds a work slot");
        let (status, message) = lightd_info().await;
        assert_eq!(status, "14", "unrouted: UNAVAILABLE");
        assert!(message.starts_with("starting"), "unrouted: {message}");

        routing.route(routes());
        let (status, message) = lightd_info().await;
        assert_eq!(status, "14", "routed, nothing served: UNAVAILABLE");
        assert!(!message.starts_with("starting"), "routed, still answered as unrouted");
    }
}
