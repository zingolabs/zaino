//! Zaino's JsonRPC Server Implementation.

use crate::{
    rpc::{jsonrpc::service::ZcashIndexerRpcServer as _, JsonRpcClient},
    server::{config::JsonRpcServerConfig, error::ServerError},
};

use zaino_state::{AtomicStatus, IndexerSubscriber, LightWalletIndexer, StatusType, ZcashIndexer};

use zebra_rpc::server::{
    cookie::{remove_from_disk, write_to_disk, Cookie},
    http_request_compatibility::HttpRequestMiddlewareLayer,
    rpc_call_compatibility::FixRpcResponseMiddleware,
};

use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder};
use std::{path::PathBuf, time::Duration};
use tokio::time::interval;
use tracing::warn;

/// JSON-RPC server capable of servicing clients over TCP.
pub struct JsonRpcServer {
    /// Current status of the server.
    pub status: AtomicStatus,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    /// Cookie dir.
    cookie_dir: Option<PathBuf>,
}

impl JsonRpcServer {
    /// Starts the JSON-RPC service.
    ///
    /// Launches all components then enters command loop:
    /// - Updates the ServerStatus.
    /// - Checks for shutdown signal, shutting down server if received.
    pub async fn spawn<Service: ZcashIndexer + LightWalletIndexer + Clone>(
        service_subscriber: IndexerSubscriber<Service>,
        server_config: JsonRpcServerConfig,
    ) -> Result<Self, ServerError> {
        let status = AtomicStatus::new(StatusType::Spawning);

        let rpc_impl = JsonRpcClient {
            service_subscriber: service_subscriber.clone(),
        };

        // Initialize Zebra-compatible cookie-based authentication if enabled.
        let (cookie, cookie_dir) = if server_config.cookie_dir.is_some() {
            let cookie = Cookie::default();
            if let Some(dir) = &server_config.cookie_dir {
                write_to_disk(&cookie, dir).map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to write cookie: {e}"))
                })?;
            } else {
                return Err(ServerError::ServerConfigError(
                    "Cookie dir must be provided when auth is enabled".into(),
                ));
            }
            (Some(cookie), server_config.cookie_dir)
        } else {
            (None, None)
        };

        // Set up Zebra HTTP request compatibility middleware (handles auth and content-type issues)
        let http_middleware_layer = HttpRequestMiddlewareLayer::new(cookie);

        // Set up Zebra JSON-RPC call compatibility middleware (RPC version fixes)
        let rpc_middleware = RpcServiceBuilder::new()
            .rpc_logger(1024)
            .layer_fn(FixRpcResponseMiddleware::new);

        // Build the JSON-RPC server with middleware integrated
        let server = ServerBuilder::default()
            .set_http_middleware(tower::ServiceBuilder::new().layer(http_middleware_layer))
            .set_rpc_middleware(rpc_middleware)
            .build(server_config.json_rpc_listen_address)
            .await
            .map_err(|e| {
                ServerError::ServerConfigError(format!("JSON-RPC server build error: {e}"))
            })?;

        let server_handle = server.start(rpc_impl.into_rpc());

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

        let task_status = status.clone();
        let server_task_handle = tokio::task::spawn({
            let server_handle_clone = server_handle.clone();
            async move {
                task_status.store(StatusType::Ready);

                tokio::select! {
                    _ = shutdown_signal => {
                        let _ = server_handle_clone.stop();
                    }
                    _ = server_handle.stopped() => {},
                }

                task_status.store(StatusType::Offline);
                Ok(())
            }
        });

        Ok(JsonRpcServer {
            status,
            server_handle: Some(server_task_handle),
            cookie_dir,
        })
    }

    /// Sets the servers to close gracefully.
    pub async fn close(&mut self) {
        self.status.store(StatusType::Closing);

        if let Some(dir) = &self.cookie_dir {
            if let Err(e) = remove_from_disk(dir) {
                warn!("Error removing cookie: {e}");
            }
        }

        if let Some(handle) = self.server_handle.take() {
            let _ = handle.await;
        }
    }

    /// Returns the servers current status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }
}

impl Drop for JsonRpcServer {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            warn!(
                "Warning: JsonRpcServer dropped without explicit shutdown. Aborting server task."
            );
        }
    }
}
