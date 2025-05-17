//! Hold error types for the Indexer and related functionality.

use zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError;
use zaino_serve::server::error::ServerError;
use zaino_state::{FetchServiceError, StateServiceError};

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
    JsonRpSeeConnectorError(#[from] JsonRpSeeConnectorError),
    /// FetchService errors.
    #[error("FetchService error: {0}")]
    FetchServiceError(#[from] FetchServiceError),
    /// FetchService errors.
    #[error("FetchService error: {0}")]
    StateServiceError(#[from] StateServiceError),
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
