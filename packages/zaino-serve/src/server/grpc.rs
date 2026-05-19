//! Zaino's gRPC Server Implementation.

use std::time::Duration;

use tokio::time::interval;
use tonic::{
    service::Routes,
    transport::{server::TcpIncoming, Server},
};
use tracing::warn;
use zaino_state::{NamedAtomicStatus, StatusType};

use crate::server::{config::GrpcServerConfig, error::ServerError};

/// LightWallet gRPC server capable of servicing clients over TCP.
pub struct TonicServer {
    /// Current status of the server.
    pub status: NamedAtomicStatus,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
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
        let status = NamedAtomicStatus::new("gRPC", StatusType::Spawning);

        let mut server_builder = Server::builder();
        if let Some(tls_config) = server_config.get_valid_tls().await? {
            server_builder = server_builder.tls_config(tls_config).map_err(|e| {
                ServerError::ServerConfigError(format!("TLS configuration error: {e}"))
            })?;
        }

        // Bind synchronously so EADDRINUSE / EACCES propagate to the caller
        // instead of being swallowed inside the spawned serve task. See
        // zingolabs/zaino#1081.
        let tcp_incoming = TcpIncoming::bind(server_config.listen_address)
            .map_err(|e| ServerError::ServerConfigError(format!("gRPC bind failed: {e}")))?;

        let shutdown_check_status = status.clone();
        let mut shutdown_check_interval = interval(Duration::from_millis(100));
        let shutdown_signal = async move {
            loop {
                shutdown_check_interval.tick().await;
                if shutdown_check_status.load() == StatusType::Closing {
                    break;
                }
            }
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
        })
    }

    /// Sets the servers to close gracefully.
    pub async fn close(&mut self) {
        self.status.store(StatusType::Closing);

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
