//! gRPC transport: a real tonic server exposed as a [`Serve`].
//!
//! Binds a `CompactTxStreamer` server over the [`GrpcService`] and serves until
//! the cancellation token fires. Implements [`Serve`] so the runtime supervises
//! it as a component. TLS and the synchronous-bind refinement (#1081) drop in
//! here later; this slice serves plaintext with tonic's graceful shutdown.

use std::net::SocketAddr;
use std::sync::Arc;

use tonic::transport::server::TcpIncoming;
use tonic::transport::Server;
use zaino_component::{CancellationToken, ReadySignal, Serve};
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zaino_service::LightServeService;

use crate::grpc::GrpcService;
use crate::LightServe;

/// A tonic `CompactTxStreamer` server over a [`LightServe`] handler.
pub struct GrpcServer<S: LightServeService + Clone> {
    handler: LightServe<S>,
    bind: SocketAddr,
}

/// Why the gRPC server could not run.
#[derive(Debug, thiserror::Error)]
pub enum GrpcServeError {
    /// The server failed to bind or its serve loop errored.
    #[error("gRPC server error: {0}")]
    Serve(String),
}

impl<S: LightServeService + Clone> GrpcServer<S> {
    /// A server exposing `handler`'s light-serve surface, bound to `bind`.
    pub fn new(handler: LightServe<S>, bind: SocketAddr) -> Self {
        Self { handler, bind }
    }
}

impl<S: LightServeService + Clone + 'static> Serve for GrpcServer<S> {
    type Error = GrpcServeError;

    async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> Result<(), GrpcServeError> {
        // Bind synchronously so a bind failure (EADDRINUSE) surfaces before we
        // report Ready, rather than being swallowed inside the serve future (#1081).
        let incoming = TcpIncoming::bind(self.bind)
            .map_err(|e| GrpcServeError::Serve(format!("bind failed: {e}")))?;
        ready.notify();
        let service = CompactTxStreamerServer::new(GrpcService::new(self.handler.clone()));
        Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(incoming, async move { cancel.cancelled().await })
            .await
            .map_err(|e| GrpcServeError::Serve(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_service::testing::{MockChain, MockIndexerService};

    /// A real tonic server binds and shuts down cleanly when the token fires.
    #[tokio::test]
    async fn binds_and_shuts_down_on_cancel() {
        let handler = LightServe::new(MockIndexerService::new(MockChain::default()));
        let server = Arc::new(GrpcServer::new(
            handler,
            "127.0.0.1:0".parse().expect("valid addr"),
        ));
        let cancel = CancellationToken::new();

        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { server.serve(cancel, ReadySignal::new(|| {})).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();

        let result = task.await.expect("join serve task");
        assert!(result.is_ok(), "clean shutdown: {result:?}");
    }
}
