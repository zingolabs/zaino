//! gRPC server implementation.
//!
//! TODO: - Add GrpcServerError error type and rewrite functions to return <Result<(), GrpcServerError>>, propagating internal errors.
//!       - Add user and password as fields of ProxyClient and use here.

use http::Uri;
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use zingo_rpc::{jsonrpc::connector::test_node_and_return_uri, primitives::client::ProxyClient};

#[cfg(not(feature = "nym_poc"))]
use zingo_rpc::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

/// Configuration data for gRPC server.
pub struct ProxyServer(pub ProxyClient);

impl ProxyServer {
    /// Starts gRPC service.
    pub fn serve(
        self,
        port: impl Into<u16> + Send + Sync + 'static,
        online: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self.0);
            let sockaddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port.into());
            println!("@zingoindexerd: gRPC server listening on: {sockaddr}");

            let server = tonic::transport::Server::builder()
                .add_service(svc.clone())
                .serve(sockaddr);

            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
            tokio::select! {
                result = server => {
                    match result {
                        Ok(_) => {
                            // TODO: Gracefully restart gRPC server.
                            println!("@zingoindexerd: gRPC Server closed early. Restart required");
                            Ok(())
                            }
                        Err(e) => {
                            // TODO: restart server or set online to false and exit
                            println!("@zingoindexerd: gRPC Server closed with error: {}. Restart required", e);
                            Err(e)
                            }
                    }
                }
                _ = async {
                    while online.load(Ordering::SeqCst) {
                        interval.tick().await;
                    }
                } => {
                    println!("@zingoindexerd: gRPC server shutting down.");
                    Ok(())
                }
            }
        })
    }

    /// Creates configuration data for gRPC server.
    pub fn new(lightwalletd_uri: http::Uri, zebrad_uri: http::Uri) -> Self {
        ProxyServer(ProxyClient {
            lightwalletd_uri,
            zebrad_uri,
            online: Arc::new(AtomicBool::new(true)),
        })
    }
}

/// Spawns a gRPC server.
pub async fn spawn_grpc_server(
    indexer_port: &u16,
    lwd_port: &u16,
    zebrad_port: &u16,
    online: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let lwd_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{lwd_port}"))
        .path_and_query("/")
        .build()
        .unwrap();

    // TODO Add user and password as fields of ProxyClient and use here.
    let zebra_uri = test_node_and_return_uri(
        zebrad_port,
        Some("xxxxxx".to_string()),
        Some("xxxxxx".to_string()),
    )
    .await
    .unwrap();

    let server = ProxyServer::new(lwd_uri, zebra_uri);
    server.serve(*indexer_port, online)
}
