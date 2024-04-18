//! gRPC server implementation.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{atomic::AtomicBool, Arc},
};

use http::Uri;
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zingo_rpc::primitives::ProxyConfig;

/// Configuration data for gRPC server.
pub struct ProxyServer {
    /// Uses zingo-rpc::primitives::ProxyConfig for consistancy across crates.
    pub proxy_config: ProxyConfig,
}

impl ProxyServer {
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

/// Spawns a gRPC server.
pub async fn spawn_server(
    proxy_port: u16,
    lwd_port: u16,
    zebrad_port: u16,
) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let lwd_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{lwd_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let zebra_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{zebrad_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let server = ProxyServer::new(lwd_uri, zebra_uri);
    server.serve(proxy_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;
    use zcash_client_backend::proto::service::Empty;

    #[tokio::test]
    /// NOTE: This test currently requires a manual boot of zcashd + lightwalletd to run
    async fn connect_to_lwd_get_info() {
        let server_port = 8080;
        let _server_handle = spawn_server(server_port, 9067, 18232).await;
        sleep(Duration::from_secs(3)).await;
        let proxy_uri = Uri::builder()
            .scheme("http")
            .authority(format!("localhost:{server_port}"))
            .path_and_query("")
            .build()
            .unwrap();
        println!("{}", proxy_uri);
        let lightd_info = zingo_netutils::GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .unwrap()
            .get_lightd_info(Empty {})
            .await
            .unwrap();
        println!("{:#?}", lightd_info.into_inner());
    }
}
