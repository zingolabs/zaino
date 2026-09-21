//! Backend error types — one per operation.
//!
//! Each variant that wraps a backend's own failure keeps it as a boxed
//! [`std::error::Error`] `#[source]`: the cause chain is preserved for
//! diagnostics, yet the port names no concrete backend type and so stays
//! backend-agnostic. A `&'static str` `operation` field names the step that
//! failed.

/// Error when obtaining a reader or writer handle.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The backend is not available (closed, corrupted, etc.). `operation` names
    /// the open step that failed.
    #[error("backend unavailable during {operation}")]
    Unavailable {
        /// The open step that failed (e.g. `"create directory"`, `"open environment"`).
        operation: &'static str,
        /// The backend's underlying error, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Error when committing a batch of write operations.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// A referenced namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    /// The database is out of space: the reserved storage is exhausted. Distinct
    /// from a generic write failure because it is neither corruption nor
    /// transient — the caller must reopen the backend with a larger map size
    /// (backed by enough disk); retrying without that fails identically.
    #[error("database out of space: the reserved storage is full; reopen the backend with a larger map size (and enough disk to back it)")]
    OutOfSpace,
    /// The write failed (IO, transaction conflict, etc.). `operation` names the
    /// write step that failed.
    #[error("write failed during {operation}")]
    WriteFailed {
        /// The low-level write step that failed (e.g. `"put"`, `"commit"`).
        operation: &'static str,
        /// The backend's underlying error, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Error when reading from the backend.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A referenced namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    /// The read failed (IO, corruption, etc.). `operation` names the read step
    /// that failed.
    #[error("read failed during {operation}")]
    ReadFailed {
        /// The read step that failed (e.g. `"get"`, `"open cursor"`).
        operation: &'static str,
        /// The backend's underlying error, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Error when flushing the backend.
#[derive(Debug, thiserror::Error)]
pub enum FlushError {
    /// The flush failed.
    #[error("flush failed")]
    IoError(#[source] Box<dyn std::error::Error + Send + Sync>),
}
