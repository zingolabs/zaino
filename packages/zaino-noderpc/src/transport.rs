//! JSON-RPC transport: a real jsonrpsee server exposed as a [`RunLoop`].
//!
//! Holds a [`NodeRpc`] handler and binds a jsonrpsee server over its
//! [`NodeRpcApiServer`](crate::NodeRpcApiServer) surface. Implements
//! [`RunLoop`] so the runtime supervises it as a component: `build` binds the
//! socket (a bind failure is the `RunLoop::Error`, not swallowed), then it serves
//! until the cancellation token fires.

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::server::ServerBuilder;
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_service::NodeRpcService;

use crate::rpc::NodeRpcApiServer;
use crate::NodeRpc;

/// A jsonrpsee server over a [`NodeRpc`] handler.
pub struct JsonRpcServer<S: NodeRpcService + Clone + 'static> {
    handler: NodeRpc<S>,
    bind: SocketAddr,
}

/// Why the JSON-RPC server could not start.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcServeError {
    /// The server could not bind / build on the configured address.
    #[error("failed to start JSON-RPC server: {0}")]
    Start(String),
}

impl<S: NodeRpcService + Clone + 'static> JsonRpcServer<S> {
    /// A server exposing `handler`'s node-RPC surface, bound to `bind`.
    pub fn new(handler: NodeRpc<S>, bind: SocketAddr) -> Self {
        Self { handler, bind }
    }
}

impl<S: NodeRpcService + Clone + 'static> RunLoop for JsonRpcServer<S> {
    type Error = JsonRpcServeError;
    const LABEL: &'static str = "serve loop";
    const RUNNING: Lifecycle = Lifecycle::Spawning;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), JsonRpcServeError> {
        // build() binds the socket, so a bind failure surfaces here as the
        // error rather than being swallowed inside the serve loop.
        let server = ServerBuilder::default()
            .build(self.bind)
            .await
            .map_err(|e| JsonRpcServeError::Start(e.to_string()))?;
        // Bound — safe to report Ready.
        reporter.ready();
        let handle = server.start(self.handler.clone().into_rpc());
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = handle.stop();
            }
            _ = handle.clone().stopped() => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_service::testing::{MockChain, MockIndexerService};

    /// A real jsonrpsee server binds and shuts down cleanly when the token fires.
    #[tokio::test]
    async fn binds_and_shuts_down_on_cancel() {
        let handler = NodeRpc::new(MockIndexerService::new(MockChain::default()));
        let server = Arc::new(JsonRpcServer::new(
            handler,
            "127.0.0.1:0".parse().expect("valid addr"),
        ));
        let cancel = CancellationToken::new();

        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { server.run(cancel, RunReporter::new(|_| {})).await }
        });
        // Let the server bind, then ask it to stop.
        tokio::task::yield_now().await;
        cancel.cancel();

        let result = task.await.expect("join serve task");
        assert!(result.is_ok(), "clean shutdown: {result:?}");
    }
}
