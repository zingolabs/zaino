//! End-to-end over the real transport: a wallet's h2 connection, and the caps around it.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use zaino_grpc::{GrpcLimits, GrpcServer, ValidatorHandler};
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

/// A real wallet gets a real answer over the hyper-served h2, and a second connection from the
/// same address is closed at accept rather than served.
#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_is_served_and_a_second_connection_from_one_address_is_refused() {
    // Bound, then released, so the server gets a port nothing else on the box holds.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = probe.local_addr().expect("local addr");
    drop(probe);

    let non_zero = |value| NonZeroUsize::new(value).expect("non-zero");
    let server = GrpcServer::new(
        ValidatorHandler::new(
            Arc::new(zaino_source::mock::MockChain::new()),
            zaino_index_compact_block::CompactBlockService::new(zaino_sync::Served::fixed(
                zaino_index_compact_block::CompactBlockStore::open(
                    zaino_persistence::fs::SimFs::new(),
                    std::path::Path::new("/cb"),
                    zcash_protocol::consensus::NetworkType::Test,
                )
                .expect("open")
                .reader()
                .pin(),
            )),
            zcash_protocol::consensus::NetworkType::Test,
        ),
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

    // Mock validator answers `getblockchaininfo` NotReady → UNAVAILABLE (no stand-in branch or
    // tip), decoded over h2 as a status rather than a dropped stream
    let refusal = wallet
        .get_lightd_info(zaino_proto::proto::service::Empty {})
        .await
        .expect_err("validator not ready");
    let named = refusal.message().starts_with("validator: ");
    assert_eq!((refusal.code(), named), (tonic::Code::Unavailable, true), "{refusal:?}");

    // The wallet holds the one per-address slot, so the next connection is closed unserved:
    // it connects (the kernel completes the handshake) and immediately reads EOF.
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

/// Behind a trusted proxy the per-address cap counts the client its PROXY header names: two
/// clients through one proxy are both served, a second connection claiming a held client is
/// closed, and a trusted peer that speaks HTTP/2 without a header is closed unserved.
#[tokio::test(flavor = "multi_thread")]
async fn behind_a_trusted_proxy_the_per_address_cap_counts_the_named_client() {
    use tokio::io::AsyncWriteExt as _;

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = probe.local_addr().expect("local addr");
    drop(probe);

    let non_zero = |value| NonZeroUsize::new(value).expect("non-zero");
    let server = GrpcServer::new(
        ValidatorHandler::new(
            Arc::new(zaino_source::mock::MockChain::new()),
            zaino_index_compact_block::CompactBlockService::new(zaino_sync::Served::fixed(
                zaino_index_compact_block::CompactBlockStore::open(
                    zaino_persistence::fs::SimFs::new(),
                    std::path::Path::new("/cb"),
                    zcash_protocol::consensus::NetworkType::Test,
                )
                .expect("open")
                .reader()
                .pin(),
            )),
            zcash_protocol::consensus::NetworkType::Test,
        ),
        bind,
        GrpcLimits { max_connections_per_ip: non_zero(1), ..GrpcLimits::default() },
    )
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

/// Held port → `bind()` errs at boot (EADDRINUSE never reaches a spawned serve loop)
#[tokio::test]
async fn binding_a_held_port_fails_before_anything_is_served() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let bind = held.local_addr().expect("local addr");

    let outcome = GrpcServer::new(
        ValidatorHandler::new(
            Arc::new(zaino_source::mock::MockChain::new()),
            zaino_index_compact_block::CompactBlockService::new(zaino_sync::Served::fixed(
                zaino_index_compact_block::CompactBlockStore::open(
                    zaino_persistence::fs::SimFs::new(),
                    std::path::Path::new("/cb"),
                    zcash_protocol::consensus::NetworkType::Test,
                )
                .expect("open")
                .reader()
                .pin(),
            )),
            zcash_protocol::consensus::NetworkType::Test,
        ),
        bind,
        GrpcLimits::default(),
    )
    .bind()
    .await;

    use zaino_grpc::GrpcServeError::Serve;
    let error = outcome.err();
    assert!(matches!(&error, Some(Serve(r)) if r.starts_with("bind failed")), "{error:?}");
}
