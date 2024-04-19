//! Wallet-side gRPC server implementation.

use http::Uri;
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{atomic::AtomicBool, Arc},
};
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zingo_rpc::primitives::ProxyConfig;

/// Configuration data for gRPC server.
pub struct NymServer {
    /// Uses zingo-rpc::primitives::ProxyConfig for consistancy across crates.
    pub proxy_config: ProxyConfig,
}

impl NymServer {
    /// Starts gRPC service.
    pub fn serve(
        self,
        port: impl Into<u16> + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        println!("Starting server task");
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self.proxy_config);
            let sockaddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port.into());
            println!("Proxy listening on {sockaddr}");
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve(sockaddr)
                .await
        })
    }

    /// Creates configuration data for gRPC server.
    pub fn new(lightwalletd_uri: http::Uri, zebrad_uri: http::Uri) -> Self {
        Self {
            proxy_config: ProxyConfig {
                lightwalletd_uri,
                zebrad_uri,
                online: Arc::new(AtomicBool::new(true)),
            },
        }
    }
}

/// Spawns a gRPC service that forwards gRPCs requests recieved to a lightwalletd.
/// Implemented RPCs are sent over the mixnet to be revieved by a nym based gRPC server (nymserverd).
pub async fn spawn_server(
    proxy_port: u16,
    lwd_port: u16,
    zebrad_port: u16,
) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let lwd_uri_test = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{lwd_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let _lwd_uri_main = Uri::builder()
        .scheme("https")
        .authority("eu.lightwalletd.com:443")
        .path_and_query("/")
        .build()
        .unwrap();
    let zebra_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{zebrad_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    // replace lwd_uri_test with lwd_uri_main to connect to mainnet:
    let server = NymServer::new(lwd_uri_test, zebra_uri);
    server.serve(proxy_port)
}
