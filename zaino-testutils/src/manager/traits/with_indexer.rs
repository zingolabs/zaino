//! Indexer state access trait.
//!
//! This trait provides access to indexer configuration and state for test managers
//! that have an indexer component running.

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use zaino_commons::config::IndexerConfig;
use zainodlib::error::IndexerError;

/// Access to indexer state and configuration.
///
/// This trait is implemented by managers that have an indexer component,
/// providing access to configuration, service endpoints, and process handles.
pub trait WithIndexer {
    /// Get the indexer configuration.
    fn indexer_config(&self) -> &IndexerConfig;

    /// Get the Zaino gRPC service address (if running).
    fn zaino_grpc_address(&self) -> Option<SocketAddr> {
        todo!("Implement zaino_grpc_address access")
    }

    /// Get the Zaino JSON-RPC service address (if running).
    fn zaino_json_address(&self) -> Option<SocketAddr> {
        todo!("Implement zaino_json_address access")  
    }

    /// Get the JSON server cookie directory (if cookie auth enabled).
    fn json_server_cookie_dir(&self) -> Option<&PathBuf> {
        todo!("Implement json_server_cookie_dir access")
    }

    /// Get the indexer process handle for monitoring.
    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>> {
        todo!("Implement indexer_handle access")
    }
}