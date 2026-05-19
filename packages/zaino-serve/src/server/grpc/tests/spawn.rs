//! Regression test for zingolabs/zaino#1081.
//!
//! `TonicServer::spawn` must perform its TCP bind synchronously so a
//! bind failure (e.g. `EADDRINUSE`) surfaces as `Err` to the caller
//! instead of being swallowed inside the spawned serve task — without
//! the synchronous bind the parent serve loop in
//! `zainod::indexer::Indexer::launch_inner` keeps running indefinitely
//! with the status board claiming everything is fine.
//!
//! This test pre-binds the gRPC port, then calls `TonicServer::spawn`
//! against the occupied address with an empty `Routes` (no real RPC
//! dispatcher — the bind fails before any request would be routed) and
//! asserts that the call returns `Err`. Before the fix the call returned
//! `Ok` and the assertion failed; after the synchronous bind landed,
//! `AddrInUse` is propagated and the assertion passes.

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;
use tonic::service::Routes;

use crate::server::{config::GrpcServerConfig, grpc::TonicServer};

#[tokio::test]
async fn returns_err_when_port_is_in_use() {
    // Occupy the port. The held listener stands in for "some other
    // process on the host already holds the gRPC port" from the issue's
    // Production impact section.
    let occupier = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind on ephemeral port must succeed");
    let occupied_addr = occupier
        .local_addr()
        .expect("local_addr on a bound listener is infallible");

    let result = TonicServer::spawn(
        Routes::default(),
        GrpcServerConfig {
            listen_address: occupied_addr,
            tls: None,
        },
    )
    .await;

    assert!(
        result.is_err(),
        "TonicServer::spawn must propagate bind failures synchronously \
         (zingolabs/zaino#1081), but returned Ok against an occupied \
         port. The bind error was swallowed inside the spawned serve task."
    );

    // If a regression returns Ok here, drop the TonicServer so its Drop
    // impl aborts the doomed serve task before the test exits.
    drop(result);
    drop(occupier);
}
