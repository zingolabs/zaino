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
    /// A put into a [`WalkOrdered`](crate::KeyOrder::WalkOrdered) namespace
    /// carried a key that is not strictly greater than the namespace's current
    /// last key.
    ///
    /// Walk-ordered namespaces are written with a sorted append, so their keys
    /// must arrive strictly ascending. A key that repeats or regresses means the
    /// index's [`KEY_ORDER`](crate::KeyOrder) claim does not match the bytes its
    /// codec produced — a codec bug, surfaced loudly on the offending commit
    /// rather than silently mis-ordering the namespace. The namespace is named so
    /// the bug is traceable to one index. A backend that does not append-order
    /// its writes never raises this.
    #[error("out-of-order key for walk-ordered namespace {namespace}: a walk-ordered namespace takes strictly ascending keys")]
    OutOfOrderAppend {
        /// The walk-ordered namespace that received the out-of-order key.
        namespace: String,
        /// The backend's underlying append rejection, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A [`Delete`](crate::WriteOp::Delete) targeted a namespace the backend was
    /// deferring in bulk mode ([`Backend::begin_bulk`](crate::Backend::begin_bulk)).
    ///
    /// A deferred namespace collects only appends into a sorted run log: the bulk
    /// load builds the append-only finalised range, which never deletes. A delete
    /// there means the caller mixed a mutation into a bulk build — a contract
    /// violation surfaced loudly on the offending commit rather than silently
    /// dropped (the log has no delete record) or silently applied to an empty
    /// tree. The namespace is named so the offending op is traceable. A backend
    /// that does not defer never raises this.
    #[error(
        "delete on deferred namespace {namespace} during bulk mode: a deferred namespace takes only appends"
    )]
    DeferredNamespaceDelete {
        /// The deferred namespace the delete targeted.
        namespace: String,
    },
    /// A deferred namespace's run log could not be decoded while completing bulk
    /// mode ([`Backend::finish_bulk`](crate::Backend::finish_bulk)): a bad segment
    /// magic or version, a checksum mismatch, or a length that runs past the
    /// committed bytes.
    ///
    /// The run-log length committed with the watermark is authoritative, so a
    /// well-formed log is never short; this variant means on-disk corruption (or a
    /// manifest/log mismatch), not a torn tail from a crash — those bytes are
    /// truncated on reopen before any read. The namespace is named and the decode
    /// error is preserved as the cause.
    #[error("corrupt deferred run log for namespace {namespace}")]
    DeferredLogCorrupt {
        /// The namespace whose run log failed to decode.
        namespace: String,
        /// The decode failure, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
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
