//! Regression test for zingolabs/zaino#1360.
//!
//! Spawning the gRPC server with TLS enabled must succeed and serve a
//! working TLS handshake. In 0.5.0, dependency feature-unification
//! enabled both of rustls's `ring` and `aws-lc-rs` provider features,
//! which defeats rustls's automatic provider selection: building the
//! TLS acceptor panicked at runtime ("Could not automatically determine
//! the process-level CryptoProvider") — but only on TLS-enabled
//! deployments, a path no test exercised because every existing test
//! ran plaintext. The fix pins the feature graph to a single provider —
//! `aws-lc-rs`, the preferred provider per ADR-0006 — and installs it
//! explicitly (`ensure_default_crypto_provider`); this test covers
//! the full TLS startup + handshake path so a future provider-selection
//! or certificate-wiring regression fails in CI instead of production.
//!
//! The certificate fixtures were minted once with openssl (EC P-256,
//! ~100-year expiry): a self-signed test CA plus a `localhost` leaf
//! (SAN `DNS:localhost` + `IP:127.0.0.1`) signed by it — rustls-webpki
//! rejects a single self-signed CA cert presented as the server's
//! end-entity cert (`CaUsedAsEndEntity`). The committed leaf key is not
//! a secret; the CA key was discarded, so regeneration remints all
//! three files. The server reads the leaf pair from disk (the
//! production `GrpcTls` path); the client trusts the CA as its root.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use tonic::service::Routes;
use tonic::transport::{server::TcpIncoming, Certificate, ClientTlsConfig, Endpoint};

use crate::server::config::{GrpcServerConfig, GrpcTls};
use crate::server::grpc::TonicServer;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/server/grpc/tests/fixtures")
        .join(name)
}

#[tokio::test]
async fn tls_spawn_serves_a_handshake() {
    // Pre-bind so the OS-assigned port is known before spawning; hand the
    // open socket to `spawn_inner` directly (this module is a child of
    // `grpc`, so the private constructor is reachable without the
    // `test_dependencies`-gated `spawn_from_listener` wrapper).
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind on ephemeral port must succeed");
    let local_addr = listener
        .local_addr()
        .expect("local_addr on a bound listener is infallible");

    let mut server = TonicServer::spawn_inner(
        Routes::default(),
        GrpcServerConfig {
            listen_address: local_addr,
            tls: Some(GrpcTls {
                cert_path: fixture_path("tls_test_cert.pem"),
                key_path: fixture_path("tls_test_key.pem"),
            }),
        },
        TcpIncoming::from(listener),
    )
    .await
    .expect("TLS-enabled spawn must succeed (zingolabs/zaino#1360)");

    let channel = Endpoint::try_from(format!("https://127.0.0.1:{}", local_addr.port()))
        .expect("loopback endpoint URI is valid")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(include_str!(
                    "fixtures/tls_test_ca_cert.pem"
                )))
                .domain_name("localhost"),
        )
        .expect("client TLS config from the fixture cert must be valid")
        .connect()
        .await
        .expect("TLS handshake against the spawned server must succeed");

    drop(channel);
    server.close().await;
}
