//! What can go wrong asking ChainHead a question, and what can go wrong
//! asking a validator one.

use zaino_primitives::types::{BlockHash, Height};

/// A ChainHead query could not be answered.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ChainHeadError {
    /// ChainHead has not yet published a snapshot, so there is nothing to
    /// answer from. Transient: it resolves once initialisation completes.
    #[error("chain head is not ready")]
    NotReady,

    /// The requested range lies outside the retained window, wholly or in part.
    ///
    /// Not a failure of ChainHead — the finalised state holds what falls below
    /// the floor. The caller routes rather than retries.
    #[error("height {requested} is below the retained floor {floor}")]
    BelowRetentionFloor {
        /// The height that was asked for.
        requested: Height,
        /// The lowest height ChainHead currently retains.
        floor: Height,
    },

    /// A range whose start is above its end.
    #[error("range start {start} is above range end {end}")]
    InvalidRange {
        /// The requested start.
        start: Height,
        /// The requested end.
        end: Height,
    },

    /// The graph is missing a block its own edges reference.
    ///
    /// An internal inconsistency, not a caller error: a published snapshot is
    /// built whole or not at all, so this means the builder has a bug.
    #[error("chain head graph is missing block {0}")]
    MissingBlock(BlockHash),
}

/// The validator could not answer a question ChainHead needed answered.
///
/// Deliberately coarse. ChainHead's response to a source failure is the same
/// in every case — back off and re-read — so the distinctions that matter are
/// "can this be retried" and "what should the operator be told", not which of
/// the underlying ports failed. The message carries the detail for the log.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ChainHeadSourceError {
    /// The validator was unreachable or returned a transport-level failure.
    /// Retry.
    #[error("validator unavailable: {0}")]
    Unavailable(String),

    /// The validator is reachable but cannot answer yet — typically still
    /// syncing. Retry.
    #[error("validator not ready: {0}")]
    NotReady(String),

    /// The validator answered, but with data that cannot be reconciled: a block
    /// missing from the chain it just reported, a parent that does not exist, a
    /// height that is not a height. Retrying may help if the validator was
    /// mid-reorg; persisting means one side is wrong.
    #[error("validator returned inconsistent data: {0}")]
    InconsistentData(String),
}
