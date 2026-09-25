//! Failures the runtime can report.

use zaino_source::{NonDomainError, SourceError, UnavailableError};

/// The chain head could not advance against its source.
///
/// The non-finalised state's `SyncError` and `UpdateError`, merged and renamed:
/// there is no sync operation here to name, and the two enums split along a
/// boundary — "extending failed" versus "publishing failed" — that no longer
/// exists now that building and publishing are separate steps.
///
/// The source arms carry the cause as `#[source]`, never a string: the chain
/// head binds the canonical ports, so what it sees from the validator is
/// already classified — retried and given up ([`Unavailable`](Self::Unavailable)),
/// not retryable ([`Transport`](Self::Transport)), or a domain rejection of a
/// query that should have succeeded ([`Rejected`](Self::Rejected)). A
/// supervisor reading the chain reaches the concrete cause.
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadAdvanceError {
    /// The validator was unreachable and the client's retry ladder is spent.
    ///
    /// Terminal for this tick. The writer reports it and tries again on the
    /// next poll; there is no second ladder here.
    #[error("validator unavailable: {0}")]
    Unavailable(#[source] UnavailableError),

    /// The validator failed the request in a way the client does not retry.
    #[error("validator request failed: {0}")]
    Transport(#[source] NonDomainError),

    /// The validator answered `query` with a domain rejection the chain head
    /// has no reading for — an absence it did not expect, or a refusal.
    ///
    /// The absences the chain head *does* expect (a block past the tip, a hash
    /// on no branch it holds) are matched by name at the call site and never
    /// reach here.
    #[error("validator rejected {query}: {source}")]
    Rejected {
        /// Which port was asked.
        query: &'static str,
        /// The port's own domain error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// The validator answered, but with data that cannot be reconciled — a
    /// block missing whose child it just served, a header whose difficulty does
    /// not decode.
    ///
    /// Retrying may help if the validator was mid-reorg; persisting means one
    /// side is wrong.
    #[error("validator returned inconsistent data: {0}")]
    InconsistentSource(String),

    /// A reorg could not be resolved within the retained window.
    #[error("reorg failed: {0}")]
    ReorgFailure(String),
}

impl ChainHeadAdvanceError {
    /// Classify a canonical port's failure, naming the port for the domain arm.
    pub(crate) fn from_source<E>(query: &'static str, error: SourceError<E>) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match error {
            SourceError::Unavailable(unavailable) => Self::Unavailable(unavailable),
            SourceError::NonDomain(cause) => Self::Transport(cause),
            SourceError::Domain(rejection) => Self::Rejected {
                query,
                source: Box::new(rejection),
            },
        }
    }
}

/// The chain head could not anchor, so it has no graph at all.
///
/// Construction is fallible because a chain head is nothing without a window:
/// it holds no persistent state to fall back on and has no other data source,
/// so one that cannot reach its validator has nothing to offer. Failing here
/// rather than existing in a degraded state is what lets `current()` be total
/// for the rest of the runtime's life.
///
/// Anchoring is one attempt: the client below has already retried, so a
/// failure here is the validator genuinely unreachable at boot, which the
/// runtime's validator gate and supervision own — not something to spin on.
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadInitError {
    /// The anchor block could not be read from the validator.
    #[error("chain head could not anchor: {0}")]
    Anchor(#[source] ChainHeadAdvanceError),
}
