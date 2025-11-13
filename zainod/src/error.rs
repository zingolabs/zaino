//! Hold error types for the Indexer and related functionality.

use zaino_fetch::jsonrpsee::error::TransportError;
use zaino_serve::server::error::ServerError;

#[allow(deprecated)]
use zaino_state::{FetchServiceError, StateServiceError};

/// Zingo-Indexer errors.
#[derive(Debug, thiserror::Error)]
#[allow(deprecated)]
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
    /// FetchService errors.
    #[error("FetchService error: {0}")]
    FetchServiceError(Box<FetchServiceError>),
    /// FetchService errors.
    #[error("StateService error: {0}")]
    StateServiceError(Box<StateServiceError>),
    /// HTTP related errors due to invalid URI.
    #[error("HTTP error: Invalid URI {0}")]
    HttpError(#[from] http::Error),
    /// Returned from tokio joinhandles..
    #[error("Join handle error: Invalid URI {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Custom indexor errors.
    #[error("Misc indexer error: {0}")]
    MiscIndexerError(String),
    /// Zaino restart signal.
    #[error("Restart Zaino")]
    Restart,
}

#[allow(deprecated)]
impl From<StateServiceError> for IndexerError {
    fn from(value: StateServiceError) -> Self {
        IndexerError::StateServiceError(Box::new(value))
    }
}

#[allow(deprecated)]
impl From<FetchServiceError> for IndexerError {
    fn from(value: FetchServiceError) -> Self {
        IndexerError::FetchServiceError(Box::new(value))
    }
}
