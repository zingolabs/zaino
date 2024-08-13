//! Hold error types for the Indexer and related functionality.

use zingo_rpc::{jsonrpc::error::JsonRpcConnectorError, server::error::ServerError};

/// Zingo-Proxy server errors.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Server based errors.
    #[error("Server error: {0}")]
    ServerError(#[from] ServerError),
    /// Configuration errors.
    #[error("Configuration error: {0}")]
    ConfigError(String),
    /// JSON RPC connector errors.
    #[error("JSON RPC connector error: {0}")]
    JsonRpcConnectorError(#[from] JsonRpcConnectorError),
    /// HTTP related errors due to invalid URI.
    #[error("HTTP error: Invalid URI {0}")]
    HttpError(#[from] http::Error),
    /// Returned from tokio joinhandles..
    #[error("Join handle error: Invalid URI {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Custom indexor errors.
    #[error("Misc indexer error: {0}")]
    MiscIndexerError(String),
}
