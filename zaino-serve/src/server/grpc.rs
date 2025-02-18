//! Zaino's gRPC Server Implementation.

use std::time::Duration;

use tokio::time::interval;
use tonic::transport::Server;
use tracing::warn;
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zaino_state::{
    fetch::FetchServiceSubscriber,
    indexer::IndexerSubscriber,
    status::{AtomicStatus, StatusType},
};

use crate::{
    rpc::GrpcClient,
    server::{config::GrpcConfig, error::ServerError},
};

/// LightWallet server capable of servicing clients over TCP.
pub struct TonicServer {
    /// Current status of the server.
    pub status: AtomicStatus,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
}

impl TonicServer {
    /// Starts the gRPC service.
    ///
    /// Launches all components then enters command loop:
    /// - Updates the ServerStatus.
    /// - Checks for shutdown signal, shutting down server if received.
    pub async fn spawn(
        service_subscriber: IndexerSubscriber<FetchServiceSubscriber>,
        server_config: GrpcConfig,
    ) -> Result<Self, ServerError> {
        let status = AtomicStatus::new(StatusType::Spawning.into());

        let svc = CompactTxStreamerServer::new(GrpcClient {
            service_subscriber: service_subscriber.clone(),
        });

        let mut server_builder = Server::builder();
        if let Some(tls_config) = server_config.get_valid_tls().await? {
            server_builder = server_builder.tls_config(tls_config).map_err(|e| {
                ServerError::ServerConfigError(format!("TLS configuration error: {}", e))
            })?;
        }

        let shutdown_check_status = status.clone();
        let mut shutdown_check_interval = interval(Duration::from_millis(100));
        let shutdown_signal = async move {
            loop {
                shutdown_check_interval.tick().await;
                if StatusType::from(shutdown_check_status.load()) == StatusType::Closing {
                    break;
                }
            }
        };
        let server_future = server_builder
            .add_service(svc)
            .serve_with_shutdown(server_config.grpc_listen_address, shutdown_signal);

        let task_status = status.clone();
        let server_handle = tokio::task::spawn(async move {
            task_status.store(StatusType::Ready.into());
            server_future.await?;
            task_status.store(StatusType::Offline.into());
            Ok(())
        });

        Ok(TonicServer {
            status,
            server_handle: Some(server_handle),
        })
    }

    /// Sets the servers to close gracefully.
    pub async fn close(&mut self) {
        self.status.store(StatusType::Closing as usize);

        if let Some(handle) = self.server_handle.take() {
            let _ = handle.await;
        }
    }

    /// Returns the servers current status.
    pub fn status(&self) -> StatusType {
        self.status.load().into()
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
