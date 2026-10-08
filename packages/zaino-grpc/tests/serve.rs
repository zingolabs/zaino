//! End-to-end over the real transport: a wallet's h2 connection + the caps around it

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use zaino_grpc::{GrpcLimits, GrpcService, Routes};
use zaino_persistence::DiskView;
use zaino_primitives::testing::MockChain;
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_source::testing::MockValidator;

/// Nothing served, chain view never polled → every `GetLightdInfo` = `UNAVAILABLE` (what these
/// transport tests read back)
fn routes() -> Routes<MockValidator, DiskView> {
    let network = zcash_protocol::consensus::NetworkType::Test;
    let chain = MockChain::regtest();
    let validator = Arc::new(MockValidator::following(&chain, chain.genesis()));
    let limits = zaino_traffic::Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = zaino_traffic::Trusted { source: validator, priority: 0, limits };
    let (validators, _never_driven) = zaino_traffic::TrafficBalancer::new(vec![trusted], None);
    let depth = zaino_primitives::types::ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
    let chain = zaino_chainview::ChainView::new(
        vec!["unpolled:18232".to_owned()],
        validators.clone(),
        depth,
    );
    let chain = chain.expect("one endpoint");
    Routes {
        snapshots: zaino_snapshot::Snapshots::fixed(None, chain.subscriber().current()),
        submit: Arc::new(chain),
        validators,
        network,
        max_address_rows: zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS,
    }
}

/// Real wallet → real answer over hyper-served h2; second connection from its address closed at
/// accept
#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_is_served_and_a_second_connection_from_one_address_is_refused() {
    // Bound then released (a port nothing else on the box holds)
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = probe.local_addr().expect("local addr");
    drop(probe);

    let non_zero = |value| NonZeroUsize::new(value).expect("non-zero");
    let server = GrpcService::new(
        routes(),
        bind,
        GrpcLimits {
            max_connections: non_zero(64),
            max_connections_per_ip: non_zero(1),
            max_streams_per_connection: NonZeroU32::new(8).expect("non-zero"),
            max_streams: non_zero(64),
            ..GrpcLimits::default()
        },
    )
    .bind()
    .await
    .expect("the freed port binds");

    let cancel = CancellationToken::new();
    let serving = tokio::spawn(server.run(cancel.clone()));

    let mut wallet = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match CompactTxStreamerClient::connect(format!("http://{bind}")).await {
                Ok(client) => return client,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("the server binds and accepts");

    // No verified tip → UNAVAILABLE (no stand-in), decoded over h2 as a status, not a dropped
    // stream
    let refusal = wallet
        .get_lightd_info(zaino_proto::proto::service::Empty {})
        .await
        .expect_err("no verified tip");
    let named = refusal.message() == "no verified header chain tip yet";
    assert_eq!((refusal.code(), named), (tonic::Code::Unavailable, true), "{refusal:?}");

    // Wallet holds the one per-address slot → next connection closed unserved (kernel handshake
    // completes, then EOF)
    let mut refused =
        tokio::net::TcpStream::connect(bind).await.expect("the listener is still accepting");
    let mut read = [0u8; 1];
    let eof = tokio::time::timeout(Duration::from_secs(5), refused.read(&mut read))
        .await
        .expect("a refused connection is closed, not held open");
    assert_eq!(eof.expect("read"), 0, "closed without a byte served");

    cancel.cancel();
    serving.await.expect("the serve task ran").expect("cancellation is a clean stop");
}

/// Behind a trusted proxy the per-address cap counts the PROXY header's client:
/// - two clients through one proxy both served
/// - second connection claiming a held client closed
/// - trusted peer speaking HTTP/2 without a header closed unserved
#[tokio::test(flavor = "multi_thread")]
async fn behind_a_trusted_proxy_the_per_address_cap_counts_the_named_client() {
    use tokio::io::AsyncWriteExt as _;

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = probe.local_addr().expect("local addr");
    drop(probe);

    let non_zero = |value| NonZeroUsize::new(value).expect("non-zero");
    let limits = GrpcLimits { max_connections_per_ip: non_zero(1), ..GrpcLimits::default() };
    let server = GrpcService::new(routes(), bind, limits)
        .with_trusted_proxies(zaino_grpc::TrustedProxies::new(vec!["127.0.0.0/8"
            .parse()
            .expect("loopback net")]))
        .bind()
        .await
        .expect("the freed port binds");

    let cancel = CancellationToken::new();
    let serving = tokio::spawn(server.run(cancel.clone()));

    // A wallet whose TCP connection opens with the proxy's header naming `client`
    let through_proxy = |client: &'static str| async move {
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
    };
    let served = |outcome: Result<tonic::Response<_>, tonic::Status>| match outcome {
        Err(status) => status.code() == tonic::Code::Unavailable,
        Ok(_) => true,
    };

    let mut first = through_proxy("203.0.113.1").await;
    let mut second = through_proxy("203.0.113.2").await;
    let empty = zaino_proto::proto::service::Empty {};
    let answered =
        [served(first.get_lightd_info(empty).await), served(second.get_lightd_info(empty).await)];
    assert_eq!(answered, [true, true], "two clients, one proxy address: both under their cap");

    let mut repeat = through_proxy("203.0.113.1").await;
    let refused = repeat.get_lightd_info(empty).await.expect_err("client already held");
    assert_ne!(refused.code(), tonic::Code::Unavailable, "closed at accept, never answered");

    let mut headerless = tokio::net::TcpStream::connect(bind).await.expect("accepting");
    headerless.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.expect("preface");
    let mut read = [0u8; 1];
    let eof = tokio::time::timeout(Duration::from_secs(5), headerless.read(&mut read))
        .await
        .expect("a headerless trusted peer is closed, not held open");
    assert_eq!(eof.expect("read"), 0, "closed without a byte served");

    cancel.cancel();
    serving.await.expect("the serve task ran").expect("cancellation is a clean stop");
}

/// Cancel closes the listener at once, then `run` drains:
/// - idle served wallet closes on its GOAWAY (no deadline wait)
/// - never-finishing connection (trusted peer silent in the 5 s PROXY read) holds `run` exactly
///   `drain_timeout`
#[tokio::test(flavor = "multi_thread")]
async fn cancel_closes_the_listener_then_waits_for_open_connections_at_most_the_drain_timeout() {
    use tokio::io::AsyncWriteExt as _;

    // case, a silent connection open, drain_timeout, run returns within
    let cases = [
        (
            "idle wallet only",
            false,
            Duration::from_secs(30),
            Duration::ZERO..Duration::from_secs(2),
        ),
        (
            "silent peer",
            true,
            Duration::from_secs(1),
            Duration::from_secs(1)..Duration::from_secs(3),
        ),
    ];
    for (case, silent, drain_timeout, returns) in cases {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let bind = probe.local_addr().expect("local addr");
        drop(probe);

        let limits = GrpcLimits { drain_timeout, ..GrpcLimits::default() };
        let server = GrpcService::new(routes(), bind, limits)
            .with_trusted_proxies(zaino_grpc::TrustedProxies::new(vec!["127.0.0.0/8"
                .parse()
                .expect("loopback net")]))
            .bind()
            .await
            .expect("the freed port binds");

        let cancel = CancellationToken::new();
        let serving = tokio::spawn(server.run(cancel.clone()));

        // Before the wallet (accept FIFO: the wallet's answer proves this one accepted)
        let _silent = match silent {
            true => Some(tokio::net::TcpStream::connect(bind).await.expect("accepting")),
            false => None,
        };
        let channel = tonic::transport::Endpoint::from_static("http://proxied")
            .connect_with_connector(tower::service_fn(move |_| async move {
                let mut stream = tokio::net::TcpStream::connect(bind).await?;
                stream.write_all(b"PROXY TCP4 203.0.113.1 127.0.0.1 40000 8137\r\n").await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }))
            .await
            .expect("the wallet connects");
        let mut wallet = CompactTxStreamerClient::new(channel);
        let empty = zaino_proto::proto::service::Empty {};
        let answered = wallet.get_lightd_info(empty).await.map_err(|status| status.code());
        assert_eq!(
            answered.err(),
            Some(tonic::Code::Unavailable),
            "{case}: served (no verified tip)"
        );

        let cancelled = std::time::Instant::now();
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let refused = tokio::net::TcpStream::connect(bind).await;
        assert!(refused.is_err(), "{case}: still accepting after cancel");
        serving.await.expect("the serve task ran").expect("cancellation is a clean stop");
        let took = cancelled.elapsed();
        assert!(returns.contains(&took), "{case}: run returned after {took:?}, not in {returns:?}");
        assert!(wallet.get_lightd_info(empty).await.is_err(), "{case}: wallet connection survived");
    }
}

/// Held port → `bind()` errs at boot (EADDRINUSE never reaches a spawned serve loop)
#[tokio::test]
async fn binding_a_held_port_fails_before_anything_is_served() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = held.local_addr().expect("local addr");

    let outcome = GrpcService::new(routes(), bind, GrpcLimits::default()).bind().await;

    use zaino_grpc::GrpcServeError::Serve;
    let error = outcome.err();
    assert!(matches!(&error, Some(Serve(r)) if r.starts_with("bind failed")), "{error:?}");
}
