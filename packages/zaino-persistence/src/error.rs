//! Backend error types.

/// Errors from backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// A write/commit/flush operation failed.
    #[error("backend write error: {0}")]
    Write(String),
    /// A read operation failed.
    #[error("backend read error: {0}")]
    Read(String),
}
