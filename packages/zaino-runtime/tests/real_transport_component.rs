//! End-to-end: a real transport server, supervised as a component.
//!
//! The full path — profile handler → transport (`Serve`) → `ServeComponent` →
//! the runtime's lifecycle — with the actual server binding a socket. `Ready` is
//! reported only once the socket is bound, so we wait for it.

use std::time::Duration;

use zaino_component::{ComponentName, Lifecycle, Managed, StatusSource, StatusWatch};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_noderpc::{JsonRpcServer, NodeRpc};
use zaino_runtime::ServeComponent;
use zaino_service::testing::{MockChain, MockIndexerService};

/// Wait until the component reports `Ready` (its socket is bound).
async fn wait_ready<C: StatusWatch>(component: &C) {
    let mut status = component.subscribe();
    loop {
        if status.borrow_and_update().lifecycle == Lifecycle::Ready {
            return;
        }
        status.changed().await.expect("status stream open");
    }
}

#[tokio::test]
async fn a_real_jsonrpc_server_boots_and_stops_as_a_component() {
    let engine = MockIndexerService::new(MockChain::default());
    let server = JsonRpcServer::new(
        NodeRpc::new(engine),
        "127.0.0.1:0".parse().expect("valid addr"),
    );
    let component = ServeComponent::new(ComponentName("node-rpc"), server);

    component.spawn().await.expect("spawn");
    tokio::time::timeout(Duration::from_secs(1), wait_ready(&component))
        .await
        .expect("became ready");
    assert_eq!(component.status().lifecycle, Lifecycle::Ready);

    component.stop().await.expect("stop");
    assert_eq!(component.status().lifecycle, Lifecycle::Offline);
}

#[tokio::test]
async fn a_real_grpc_server_boots_and_stops_as_a_component() {
    let engine = MockIndexerService::new(MockChain::default());
    let server = GrpcServer::new(
        LightServe::new(engine),
        "127.0.0.1:0".parse().expect("valid addr"),
    );
    let component = ServeComponent::new(ComponentName("light-serve"), server);

    component.spawn().await.expect("spawn");
    tokio::time::timeout(Duration::from_secs(1), wait_ready(&component))
        .await
        .expect("became ready");
    assert_eq!(component.status().lifecycle, Lifecycle::Ready);

    component.stop().await.expect("stop");
    assert_eq!(component.status().lifecycle, Lifecycle::Offline);
}
