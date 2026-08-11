//! What can go wrong asking the chain store a question, and what can go wrong
//! asking a validator one.

use zaino_primitives::types::Height;

use crate::capability::StoreCapability;

/// A chain-store query could not be answered.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
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
    #[error("chain store is missing {0}")]
    MissingRow(String),

    /// The storage backend failed.
    ///
    /// Opaque by intent: the message is for an operator's log, and domain
    /// logic must not branch on its contents. A backend that wants a failure
    /// distinguished should surface it as its own variant here rather than
    /// encoding it in this string.
    #[error("chain store backend failed: {0}")]
    Backend(String),
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
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ChainStoreSourceError {
    /// The validator was unreachable or returned a transport-level failure.
    /// Retry.
    #[error("validator unavailable: {0}")]
    Unavailable(String),

    /// The validator is reachable but cannot answer yet — typically still
    /// syncing past the height the store asked for. Retry.
    #[error("validator not ready: {0}")]
    NotReady(String),

    /// The validator answered with data the store cannot reconcile: a block
    /// whose parent is not the block below it, a height that returns a
    /// different block than it did a moment ago. Retrying may help if the
    /// validator was mid-reorg; persisting means one side is wrong.
    #[error("validator returned inconsistent data: {0}")]
    InconsistentData(String),

    /// The store could not commit what it built.
    ///
    /// Separate from the validator variants because the remedy differs: the
    /// data arrived, and it is the local side that failed.
    #[error("chain store could not commit: {0}")]
    Commit(String),
}
