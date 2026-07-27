//! Backend error types — one per operation.

/// Error when obtaining a reader or writer handle.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The backend is not available (closed, corrupted, etc.).
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}

/// Error when committing a batch of write operations.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// A referenced namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    /// The write failed (IO, transaction conflict, etc.).
    #[error("write failed: {0}")]
    WriteFailed(String),
}

/// Error when reading from the backend.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A referenced namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    /// The read failed (IO, corruption, etc.).
    #[error("read failed: {0}")]
    ReadFailed(String),
}

/// Error when flushing the backend.
#[derive(Debug, thiserror::Error)]
pub enum FlushError {
    /// The flush failed (IO error).
    #[error("flush failed: {0}")]
    IoError(String),
}
