//! Backend error types — one per operation.

/// Error when obtaining a reader or writer handle.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The backend is not available (closed, corrupted, etc.). The backend's
    /// own error is kept as a typed `source` (boxed so the port names no
    /// concrete backend type), with `operation` naming the open step that
    /// failed.
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
    /// The database is out of space: the backend's reserved storage is
    /// exhausted. Modelled distinctly from a generic write failure because it
    /// is neither corruption nor transient — the caller must give the database
    /// more room (a larger configured map size, backed by enough disk) and
    /// retrying without doing so will fail identically.
    #[error("database out of space: the reserved storage is full; reopen the backend with a larger map size (and enough disk to back it)")]
    OutOfSpace,
    /// The write failed (IO, transaction conflict, etc.). The backend's own
    /// error is kept as a typed `source` — inspectable via
    /// [`Error::source`](std::error::Error::source), not flattened into a
    /// string — while `operation` names the write step that failed. `source`
    /// is boxed because this is the backend-agnostic port: it must not name a
    /// concrete backend's error type (that would recouple the port to one
    /// adapter), yet the cause chain is preserved for diagnostics.
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
    /// The read failed (IO, corruption, etc.). The backend's own error is kept
    /// as a typed `source` (boxed so the port names no concrete backend type),
    /// with `operation` naming the read step that failed.
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
    /// The flush failed. The backend's own error is kept as a typed `source`
    /// (boxed so the port names no concrete backend type), not stringified.
    #[error("flush failed")]
    IoError(#[source] Box<dyn std::error::Error + Send + Sync>),
}
