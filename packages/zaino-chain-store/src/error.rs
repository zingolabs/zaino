//! What can go wrong asking the chain store a question, and what can go wrong
//! asking a validator one.

use zaino_primitives::types::Height;

use crate::capability::StoreCapability;

/// An opaque cause, kept for the operator's log.
///
/// Boxed because the domain must not name the backend's error type: an LMDB
/// errno and a validator's RPC failure are both causes here, and neither is
/// something this crate can depend on. Erasing the type keeps the cause
/// reportable without making it branchable, which is the same contract
/// [`ChainStoreError::Backend`] already had for its message.
pub type BoxCause = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A chain-store query could not be answered.
///
/// # Why this type is not `Clone`, `PartialEq` or `Eq`
///
/// It carries its causes as [`BoxCause`], which is none of those things. The
/// derives came first and forced every cause to be flattened into a `String`,
/// which left [`std::error::Error::source`] returning `None` for the variants
/// whose whole purpose is telling an operator what broke. Reporting the cause
/// is worth more than comparing or copying the error, so the derives went and
/// the causes stayed.
///
/// Tests that used to compare two errors with `==` match on the variant
/// instead, which is what they were really asserting.
#[derive(Debug, thiserror::Error)]
pub enum ChainStoreError {
    /// The store has not finished opening, so there is nothing to answer from.
    /// Transient: it resolves once initialisation completes.
    #[error("chain store is not ready")]
    NotReady,

    /// The requested height is above the finalised watermark.
    ///
    /// Not a miss. The block may well exist — the chain head holds what sits
    /// above the watermark — so a caller that treats this as "no such block"
    /// will report absent data as absent chain. The caller routes rather than
    /// retries.
    #[error("height {requested} is above the finalised watermark {watermark}")]
    AboveWatermark {
        /// The height that was asked for.
        requested: Height,
        /// The highest height the store can currently answer for.
        watermark: Height,
    },

    /// A range whose start is above its end.
    ///
    /// Ranges are ascending. A caller wanting descending order reverses the
    /// result; the store does not walk backwards, because doing so doubles
    /// every range path for a case only one consumer has.
    #[error("range start {start} is above range end {end}")]
    InvalidRange {
        /// The requested start.
        start: Height,
        /// The requested end.
        end: Height,
    },

    /// This store does not offer the capability the query needs.
    ///
    /// Distinct from a miss and from a failure: the store is healthy and the
    /// question is well-formed, but this deployment does not build that index,
    /// or has not finished building it. A caller either routes elsewhere or
    /// reports the capability as unavailable — retrying will not help.
    #[error("chain store does not currently offer {0}")]
    Unavailable(StoreCapability),

    /// A row the store's own indexes reference is missing.
    ///
    /// An internal inconsistency, not a caller error: the store writes a block
    /// and its index entries in one transaction, so a dangling reference means
    /// either a bug or corruption on disk.
    ///
    /// Not for a row that is present but holds an unusable value — that is
    /// [`ChainStoreError::CorruptRow`], and the two want different repairs.
    #[error("chain store is missing {0}")]
    MissingRow(String),

    /// A row is present and readable, but holds a value the domain cannot
    /// express: a height above the protocol maximum, an amount above the money
    /// supply, a tag naming no script type.
    ///
    /// Distinct from [`ChainStoreError::MissingRow`], which says an index
    /// points at a row that is not there. Here the index is intact and the row
    /// is where it should be; what is wrong is inside it. The distinction is
    /// the operator's, and it is the repair that differs: a dangling index
    /// entry is rebuilt from the rows it references, while a corrupt value
    /// means the row itself has to be refetched from a validator and rewritten.
    /// Reporting one as the other sends that operator down the wrong path.
    #[error("chain store holds a corrupt row: expected {expected}")]
    CorruptRow {
        /// What the row should have held, in the terms the reader expected it.
        expected: String,
        /// Why it could not be read, when the conversion had a typed error.
        #[source]
        cause: Option<BoxCause>,
    },

    /// The storage backend failed.
    ///
    /// Opaque by intent: the message is for an operator's log, and domain
    /// logic must not branch on its contents. A backend that wants a failure
    /// distinguished should surface it as its own variant here rather than
    /// encoding it in this string.
    ///
    /// Opaque to *branching*, though — not to reading. `cause` carries the
    /// backend's own error so the chain survives into a log; without it an
    /// operator gets the summary of the one failure whose entire job is
    /// explaining itself.
    #[error("chain store backend failed: {message}")]
    Backend {
        /// What failed, for an operator.
        message: String,
        /// Why, when the backend had a typed error to hand over.
        #[source]
        cause: Option<BoxCause>,
    },
}

impl ChainStoreError {
    /// A row holding a value the domain cannot express, with no typed cause.
    ///
    /// For a value that is simply absent or unrecognised, where there was no
    /// conversion to fail — a tag matching no known variant, say.
    pub fn corrupt_row(expected: impl Into<String>) -> Self {
        Self::CorruptRow {
            expected: expected.into(),
            cause: None,
        }
    }

    /// A row holding a value the domain cannot express, because of `cause`.
    ///
    /// The conversion that rejected the value knows exactly why — which bound
    /// was exceeded, and by what — and that is the part an operator needs in
    /// order to tell corruption from a protocol change.
    pub fn corrupt_row_because(
        expected: impl Into<String>,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::CorruptRow {
            expected: expected.into(),
            cause: Some(Box::new(cause)),
        }
    }

    /// A backend failure described by `message`, with no cause to hand over.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
            cause: None,
        }
    }

    /// A backend failure described by `message` and caused by `cause`.
    ///
    /// Takes both because the message is usually not the cause's `Display`:
    /// the backend knows which block or height it was reading, and that
    /// context is what makes the log entry actionable.
    pub fn backend_because(
        message: impl Into<String>,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Backend {
            message: message.into(),
            cause: Some(Box::new(cause)),
        }
    }
}

/// The validator could not answer a question the store needed answered.
///
/// Deliberately coarse, and deliberately separate from [`ChainStoreError`]:
/// this is what goes wrong while the store *builds itself*, where a query
/// error is what goes wrong while it answers. A reader never produces one.
///
/// The store's response to a source failure is the same in every case — back
/// off and retry — so the distinctions that matter are "can this be retried"
/// and "what should the operator be told", not which port failed.
///
/// Not `Clone`, `PartialEq` or `Eq`, for the reason given on
/// [`ChainStoreError`]: the causes are boxed so they survive into a log.
#[derive(Debug, thiserror::Error)]
pub enum ChainStoreSourceError {
    /// The validator was unreachable or returned a transport-level failure.
    /// Retry.
    #[error("validator unavailable: {message}")]
    Unavailable {
        /// What failed, for an operator.
        message: String,
        /// Why, when the caller had a typed error to hand over.
        #[source]
        cause: Option<BoxCause>,
    },

    /// The validator is reachable but cannot answer yet — typically still
    /// syncing past the height the store asked for. Retry.
    #[error("validator not ready: {message}")]
    NotReady {
        /// What failed, for an operator.
        message: String,
        /// Why, when the caller had a typed error to hand over.
        #[source]
        cause: Option<BoxCause>,
    },

    /// The validator answered with data the store cannot reconcile: a block
    /// whose parent is not the block below it, a height that returns a
    /// different block than it did a moment ago. Retrying may help if the
    /// validator was mid-reorg; persisting means one side is wrong.
    #[error("validator returned inconsistent data: {message}")]
    InconsistentData {
        /// What failed, for an operator.
        message: String,
        /// Why, when the caller had a typed error to hand over.
        #[source]
        cause: Option<BoxCause>,
    },

    /// The store could not commit what it built.
    ///
    /// Separate from the validator variants because the remedy differs: the
    /// data arrived, and it is the local side that failed.
    #[error("chain store could not commit: {message}")]
    Commit {
        /// What failed, for an operator.
        message: String,
        /// Why, when the caller had a typed error to hand over.
        #[source]
        cause: Option<BoxCause>,
    },
}

impl ChainStoreSourceError {
    /// The validator was unreachable, with no cause to hand over.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
            cause: None,
        }
    }

    /// The validator cannot answer yet, with no cause to hand over.
    pub fn not_ready(message: impl Into<String>) -> Self {
        Self::NotReady {
            message: message.into(),
            cause: None,
        }
    }

    /// The validator's answer does not reconcile, with no cause to hand over.
    pub fn inconsistent_data(message: impl Into<String>) -> Self {
        Self::InconsistentData {
            message: message.into(),
            cause: None,
        }
    }

    /// The store could not commit, with no cause to hand over.
    pub fn commit(message: impl Into<String>) -> Self {
        Self::Commit {
            message: message.into(),
            cause: None,
        }
    }

    /// The store could not commit, because of `cause`.
    pub fn commit_because(
        message: impl Into<String>,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Commit {
            message: message.into(),
            cause: Some(Box::new(cause)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainStoreError, ChainStoreSourceError};
    use std::error::Error as _;

    /// A stand-in for whatever a backend fails with.
    #[derive(Debug, thiserror::Error)]
    #[error("errno 22")]
    struct BackendFailure;

    /// A backend failure carries its cause to the operator.
    ///
    /// The point of dropping the `Clone`/`Eq` derives: before, the cause could
    /// only be flattened into the message and `source()` returned `None` for
    /// the one variant whose whole job is explaining what broke.
    #[test]
    fn a_backend_failure_reports_why() {
        let error = ChainStoreError::backend_because("reading block 42", BackendFailure);

        assert_eq!(
            error.to_string(),
            "chain store backend failed: reading block 42"
        );
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("errno 22".to_string()),
            "the backend's own error should survive into the log"
        );
    }

    /// A backend failure with nothing to hand over says so.
    ///
    /// `None` rather than a cause repeating the message: a chain that ends is
    /// more honest than one that pads itself.
    #[test]
    fn a_backend_failure_without_a_cause_has_no_source() {
        let error = ChainStoreError::backend("reading block 42");

        assert!(error.source().is_none());
    }

    /// A failed commit carries its cause too.
    #[test]
    fn a_failed_commit_reports_why() {
        let error = ChainStoreSourceError::commit_because("writing block 42", BackendFailure);

        assert_eq!(
            error.source().map(ToString::to_string),
            Some("errno 22".to_string())
        );
    }
}
