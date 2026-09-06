//! Zaino's gRPC Server Implementation.

use tokio::sync::watch;
use tonic::{
    service::Routes,
    transport::{server::TcpIncoming, Server},
};
use tracing::warn;
use zaino_status::{NamedAtomicStatus, StatusType};

use crate::server::{config::GrpcServerConfig, error::ServerError};

/// LightWallet gRPC server capable of servicing clients over TCP.
pub struct TonicServer {
    /// Current status of the server.
    pub status: NamedAtomicStatus,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    shutdown: watch::Sender<()>,
}

impl TonicServer {
    /// Starts the gRPC service.
    ///
    /// `routes` is a pre-assembled tonic service router; production
    /// callers build one from an indexer subscriber via
    /// [`crate::rpc::grpc_routes`]. Decoupling the dispatcher from the
    /// transport layer keeps this function focused on bind / TLS /
    /// shutdown and lets the bind-race regression test (see
    /// zingolabs/zaino#1081) pass [`Routes::default`] instead of a full
    /// trait-stubbed indexer.
    pub async fn spawn(
        routes: Routes,
        server_config: GrpcServerConfig,
    ) -> Result<Self, ServerError> {
        Self::spawn_with_routes(|_| routes, server_config).await
    }

    /// Starts the gRPC service and gives its routes the server shutdown signal.
    pub async fn spawn_with_routes<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        // Bind synchronously so EADDRINUSE / EACCES propagate to the caller
        // instead of being swallowed inside the spawned serve task. See
        // zingolabs/zaino#1081.
        let tcp_incoming = TcpIncoming::bind(server_config.listen_address)
            .map_err(|e| ServerError::ServerConfigError(format!("gRPC bind failed: {e}")))?;
        Self::spawn_inner(build_routes, server_config, tcp_incoming).await
    }

    /// Starts the gRPC service on a pre-bound listener.
    ///
    /// Lets a test harness bind `127.0.0.1:0`, read the OS-assigned port, and
    /// hand the still-open socket here — closing the pick-a-port / bind-later
    /// race. `TcpIncoming::from` applies the same nodelay/keepalive defaults as
    /// `TcpIncoming::bind`, so the served socket is identical to the production
    /// path.
    #[cfg(any(test, feature = "test_dependencies"))]
    pub async fn spawn_from_listener(
        routes: Routes,
        server_config: GrpcServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError> {
        Self::spawn_from_listener_with_routes(|_| routes, server_config, listener).await
    }

    /// Starts the gRPC service on a pre-bound listener and shares its shutdown signal.
    #[cfg(any(test, feature = "test_dependencies"))]
    pub async fn spawn_from_listener_with_routes<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        listener.set_nonblocking(true).map_err(|e| {
            ServerError::ServerConfigError(format!("gRPC listener set_nonblocking failed: {e}"))
        })?;
        let tcp_incoming =
            TcpIncoming::from(tokio::net::TcpListener::from_std(listener).map_err(|e| {
                ServerError::ServerConfigError(format!("gRPC from_std failed: {e}"))
            })?);
        Self::spawn_inner(build_routes, server_config, tcp_incoming).await
    }

    async fn spawn_inner<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
        tcp_incoming: TcpIncoming,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        let status = NamedAtomicStatus::new("gRPC", StatusType::Spawning);
        let (shutdown, mut shutdown_signal) = watch::channel(());
        let routes = build_routes(shutdown.subscribe());

        let mut server_builder = Server::builder();
        if let Some(tls_config) = server_config.get_valid_tls().await? {
            // Building the TLS acceptor requires a process-level rustls
            // CryptoProvider (zingolabs/zaino#1360).
            zaino_common::crypto::ensure_default_crypto_provider();
            server_builder = server_builder.tls_config(tls_config).map_err(|e| {
                ServerError::ServerConfigError(format!("TLS configuration error: {e}"))
            })?;
        }

        let shutdown_signal = async move {
            let _ = shutdown_signal.changed().await;
        };
        let server_future = server_builder
            .add_routes(routes)
            .serve_with_incoming_shutdown(tcp_incoming, shutdown_signal);

        let task_status = status.clone();
        let server_handle = tokio::task::spawn(async move {
            task_status.store(StatusType::Ready);
            server_future.await?;
            task_status.store(StatusType::Offline);
            Ok(())
        });

        Ok(TonicServer {
            status,
            server_handle: Some(server_handle),
            shutdown,
        })
    }

    /// Sets the servers to close gracefully.
    pub async fn close(&mut self) {
        self.status.store(StatusType::Closing);
        self.shutdown.send_replace(());

        if let Some(handle) = self.server_handle.take() {
            let _ = handle.await;
        }
    }

    /// Returns the servers current status.
    ///
    /// If the spawned serve task has finished (panic, tonic-internal
    /// error, etc.), reports `Offline` regardless of the cached status —
    /// otherwise a serve task that died after reporting `Ready` would
    /// keep the indexer's critical-error check from firing. See
    /// zingolabs/zaino#1081.
    pub fn status(&self) -> StatusType {
        if self.server_handle.as_ref().is_some_and(|h| h.is_finished()) {
            return StatusType::Offline;
        }
        self.status.load()
    }
}

impl Drop for TonicServer {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            warn!("Warning: TonicServer dropped without explicit shutdown. Aborting server task.");
        }
    }
}

#[cfg(test)]
mod tests;
