//! Hold error types for the Indexer and related functionality.

use zaino_fetch::jsonrpsee::error::TransportError;
use zaino_serve::server::error::ServerError;

use zaino_state::NodeBackedIndexerServiceError;

/// Zingo-Indexer errors.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Server based errors.
    #[error("Server error: {0}")]
    ServerError(#[from] ServerError),
    /// Configuration errors.
    #[error("Configuration error: {0}")]
    ConfigError(String),
    /// JSON RPSee connector errors.
    #[error("JSON RPSee connector error: {0}")]
    TransportError(#[from] TransportError),
    /// NodeBackedIndexerService errors.
    #[error("NodeBackedIndexerService error: {0}")]
    NodeBackedIndexerServiceError(Box<NodeBackedIndexerServiceError>),
    /// HTTP related errors due to invalid URI.
    #[error("HTTP error: Invalid URI {0}")]
    HttpError(#[from] http::Error),
    /// Returned from tokio joinhandles..
    #[error("Join handle error: Invalid URI {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Custom indexor errors.
    #[error("Misc indexer error: {0}")]
    MiscIndexerError(String),
    /// Metrics endpoint errors.
    #[cfg(feature = "prometheus")]
    #[error("Metrics error: {0}")]
    MetricsError(String),
    /// Zaino restart signal.
    #[error("Restart Zaino")]
    Restart,
}

impl From<NodeBackedIndexerServiceError> for IndexerError {
    fn from(value: NodeBackedIndexerServiceError) -> Self {
        IndexerError::NodeBackedIndexerServiceError(Box::new(value))
    }
}
