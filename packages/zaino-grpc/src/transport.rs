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
        let dispatch = Dispatch::new(routes, &limits);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zaino_persistence::IndexKind;
    use zaino_primitives::testing::{p2pkh, MockChain};
    use zaino_proto::proto::service as proto;
    use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

    use super::GrpcService;
    use crate::service::Routes;
    use crate::testing::{indexed, routes, snapshot, MAINNET};

    /// R11, W4: a wallet on an h2 >= 0.4.16 client (charges every DATA frame < 256 B against a
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
        let routes = Routes { snapshots: snapshot(&chain, tip, views), ..routes() };

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let bind = probe.local_addr().expect("local addr");
        drop(probe);
        let server = GrpcService::new(routes, bind, crate::GrpcLimits::default());
        let bound = server.bind().await.expect("the freed port binds");
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

        let request = proto::GetAddressUtxosArg {
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

        let at = |height| Some(proto::BlockId { height, hash: Vec::new() });
        let range = proto::BlockRange { start: at(0), end: at(9_999), pool_types: Vec::new() };
        let mut range = wallet.get_block_range(range).await.expect("opens").into_inner();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut heights = Vec::new();
        while let Some(block) = range.message().await.expect("no GOAWAY mid-stream") {
            heights.push(block.height);
        }
        assert_eq!(heights, (0..u64::from(COUNT)).collect::<Vec<_>>(), "every block, in order");
        cancel.cancel();
    }
}
