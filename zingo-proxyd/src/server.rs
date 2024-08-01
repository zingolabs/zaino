//! gRPC server implementation.
//!
//! TODO: - Add GrpcServerError error type and rewrite functions to return <Result<(), GrpcServerError>>, propagating internal errors.
//!       - Add user and password as fields of ProxyClient and use here.

// use http::Uri;
// use std::{
//     net::{Ipv4Addr, SocketAddr},
//     sync::{
//         atomic::{AtomicBool, Ordering},
//         Arc,
//     },
// };

use http::Uri;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use tonic::codegen::{BoxFuture, StdError};
use tonic::transport::NamedService;
use tower::Service;

use zingo_rpc::{jsonrpc::connector::test_node_and_return_uri, rpc::GrpcClient};

#[cfg(not(feature = "nym_poc"))]
use zingo_rpc::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;

/// Configuration data for gRPC server.
pub struct GrpcServer(pub GrpcClient);

impl GrpcServer {
    /// Starts gRPC service.
    pub fn serve(
        self,
        port: impl Into<u16> + Send + Sync + 'static,
        online: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self.0);
            let logging_svc = LoggingService::new(svc);

            let sockaddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port.into());
            println!("@zingoproxyd: gRPC server listening on: {sockaddr}");

            let server = tonic::transport::Server::builder()
                .add_service(logging_svc.clone())
                .serve(sockaddr);

            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
            tokio::select! {
                result = server => {
                    match result {
                        Ok(_) => {
                            // TODO: Gracefully restart gRPC server.
                            println!("@zingoproxyd: gRPC Server closed early. Restart required");
                            Ok(())
                            }
                        Err(e) => {
                            // TODO: restart server or set online to false and exit
                            println!("@zingoproxyd: gRPC Server closed with error: {}. Restart required", e);
                            Err(e)
                            }
                    }
                }
                _ = async {
                    while online.load(Ordering::SeqCst) {
                        interval.tick().await;
                    }
                } => {
                    println!("@zingoproxyd: gRPC server shutting down.");
                    Ok(())
                }
            }
        })
    }

    /// Creates configuration data for gRPC server.
    pub fn new(lightwalletd_uri: http::Uri, zebrad_uri: http::Uri) -> Self {
        GrpcServer(GrpcClient {
            lightwalletd_uri,
            zebrad_uri,
            online: Arc::new(AtomicBool::new(true)),
        })
    }
}

/// Spawns a gRPC server.
pub async fn spawn_grpc_server(
    proxy_port: &u16,
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

    let server = GrpcServer::new(lwd_uri, zebra_uri);
    server.serve(*proxy_port, online)
}

#[derive(Clone)]
struct LoggingService<T> {
    inner: T,
}

impl<T> LoggingService<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T, B> Service<http::Request<B>> for LoggingService<T>
where
    T: Service<http::Request<B>, Response = http::Response<tonic::body::BoxBody>> + Send + 'static,
    B: Send + 'static + std::fmt::Debug,
    T::Error: Into<StdError> + Send + 'static,
    T::Future: Send + 'static,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        println!("Received request: {:?}", req);
        let fut = self.inner.call(req);
        Box::pin(async move {
            let res = fut.await?;
            Ok(res)
        })
    }
}

impl<T> NamedService for LoggingService<T>
where
    T: NamedService,
{
    const NAME: &'static str = T::NAME;
}
