//! Hold error types for the server and related functionality.

/// Zingo-Indexer server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Server configuration errors.
    #[error("Server configuration error: {0}")]
    ServerConfigError(String),

    /// Errors returned by Tonic's transport layer.
    #[error("Tonic transport error: {0}")]
    TonicTransportError(#[from] tonic::transport::Error),
}
