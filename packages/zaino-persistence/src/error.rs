//! Backend error types — one per operation.
//!
//! Each variant that wraps a backend's own failure keeps it as a boxed
//! [`std::error::Error`] `#[source]`: the cause chain is preserved for
//! diagnostics, yet the port names no concrete backend type and so stays
//! backend-agnostic. A `&'static str` `operation` field names the step that
//! failed.

use core::num::NonZeroU32;

use zaino_primitives::types::BlockRef;

/// Error when obtaining a reader or writer handle.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Two specs declare the same namespace.
    #[error("namespace declared twice: {0}")]
    DuplicateNamespace(String),
    /// The storage was written as another index: another name, format or
    /// namespaces than this open declares.
    #[error("storage written as another index")]
    Mismatch(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Committed data is lost or fails its integrity check: verify, resync.
    #[error("stored data lost or corrupted, found at open")]
    Corrupted(#[source] Box<dyn std::error::Error + Send + Sync>),
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
    /// The disk or quota is full. Neither corruption nor transient: the backend
    /// refuses writes; free space, then reopen.
    #[error("out of space: the disk or quota is full; free space, then reopen")]
    OutOfSpace(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// An item's length differs from its namespace's declared width.
    #[error("{namespace}: a {actual}-byte {part}, declared {expected}")]
    WidthMismatch {
        /// The namespace.
        namespace: String,
        /// Which part of the op.
        part: Part,
        /// Declared width.
        expected: NonZeroU32,
        /// Bytes given.
        actual: usize,
    },
    /// A sequence `Put` not at the next position.
    #[error("{namespace}: append at {got:?}, next position is {expected}")]
    NotNextPosition {
        /// The namespace.
        namespace: String,
        /// The next position.
        expected: u64,
        /// Position the key names (`None` = not a sequence key).
        got: Option<u64>,
    },
    /// A `Delete` in a sequence, or in a map declared without `deletes`.
    #[error("{namespace}: deletes not allowed")]
    DeleteNotAllowed {
        /// The namespace.
        namespace: String,
    },
    /// One batch names a map key twice.
    #[error("{namespace}: a key written twice in one batch")]
    DuplicateInBatch {
        /// The namespace.
        namespace: String,
    },
    /// A `Put` of a map key already present.
    #[error("{namespace}: key already present")]
    KeyExists {
        /// The namespace.
        namespace: String,
    },
    /// A `Put` or `Delete` of a map key already removed.
    #[error("{namespace}: key already removed")]
    KeyRemoved {
        /// The namespace.
        namespace: String,
    },
    /// A `Delete` of a map key not present.
    #[error("{namespace}: key absent")]
    KeyAbsent {
        /// The namespace.
        namespace: String,
    },
    /// The commit's tip is not above the last committed tip.
    #[error("tip {tip:?} not above the last committed {last:?}")]
    TipNotAbove {
        /// The commit's tip.
        tip: BlockRef,
        /// The last committed tip.
        last: BlockRef,
    },
    /// The backend's commit record cannot hold another commit.
    #[error("commit record full")]
    ManifestFull,
    /// A previous write failed; the backend refuses writes until reopened.
    #[error("a previous write failed: reopen the backend")]
    Poisoned,
    /// The backend's background merge failed, or failed to start: nothing of
    /// this write was applied, the backend stays usable and merges again later.
    #[error("a background merge failed; nothing applied")]
    MergeFailed(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Stored bytes failed their integrity check while committing (checking the
    /// batch, or a merge's input): the backend refuses writes; stop, verify,
    /// resync.
    #[error("stored data corrupted, found while writing")]
    Corrupted(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Reading the state a commit is checked against failed.
    #[error("commit check failed")]
    Check(#[source] ReadError),
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

impl CommitError {
    /// Reading the state a commit is checked against failed: stored bytes found
    /// corrupt are [`CommitError::Corrupted`] (stop, verify, resync), anything
    /// else [`CommitError::Check`].
    pub fn from_check(error: ReadError) -> Self {
        match error {
            ReadError::Corrupted { .. } => Self::Corrupted(Box::new(error)),
            error => Self::Check(error),
        }
    }
}

/// Error when reading from the backend.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A referenced namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    /// A point read's key is not of its namespace's key width.
    #[error("{namespace}: a {actual}-byte key, declared {expected}")]
    WidthMismatch {
        /// The namespace.
        namespace: String,
        /// Declared key width (a sequence key is 8 bytes).
        expected: usize,
        /// Bytes given.
        actual: usize,
    },
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
    /// Stored bytes fail their integrity check: stop, verify, resync.
    #[error("stored data corrupted, found during {operation}")]
    Corrupted {
        /// The read step that found it.
        operation: &'static str,
        /// The backend's finding, preserved as the cause.
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
    /// Writing the buffered commits failed.
    #[error("writing buffered commits failed")]
    Write(#[source] CommitError),
}

/// Error when verifying the backend's stored data.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The check could not run. `operation` names the step that failed.
    #[error("verify failed during {operation}")]
    Failed {
        /// The step that failed.
        operation: &'static str,
        /// The backend's underlying error, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Error when measuring the backend's footprint.
#[derive(Debug, thiserror::Error)]
pub enum FootprintError {
    /// The measurement failed. `operation` names the step that failed.
    #[error("footprint failed during {operation}")]
    Failed {
        /// The step that failed.
        operation: &'static str,
        /// The backend's underlying error, preserved as the cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Which part of a write op a width applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// A map key.
    Key,
    /// A map value.
    Value,
    /// A sequence record.
    Record,
}

impl core::fmt::Display for Part {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Key => "key",
            Self::Value => "value",
            Self::Record => "record",
        })
    }
}
