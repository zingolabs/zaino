//! Why a poll, a query, or a broadcast could not complete.

use zaino_source::{NonDomainError, SendRawTransactionError};

/// Rejected endpoint list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no validator endpoints configured")]
    NoEndpoints,

    #[error("{count} endpoints configured, {max} is the ceiling", max = crate::EndpointSet::MAX)]
    TooManyEndpoints { count: usize },
}

/// A failed poll of one endpoint.
///
/// Two classes, split on retryability:
///
/// - [`Unavailable`](Self::Unavailable) — statement about the *node*: `Down` at once
/// - [`Source`](Self::Source) — transport or a garbled answer: backoff, `Down` at the failure
///   ceiling
///
/// `GetRawMempoolTransactionError::NotFound` is neither: the listing/fetch race is normal, and
/// the poller skips that txid.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EndpointPollError {
    #[error("validator exposes no mempool")]
    Unavailable,

    #[error(transparent)]
    Source(#[from] NonDomainError),
}

/// Final headers not durable: header sync ends, the process with it (never warn-and-continue)
#[derive(Debug, thiserror::Error)]
#[error("header store commit failed: {0}")]
pub struct HeaderStoreFailed(#[from] zaino_persistence::StoreError);

/// A submission no trusted validator accepted or listed (§6)
///
/// - one rejecting, another accepting = success (a refusal is usually local policy)
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// The precheck's refusal, or the first trusted validator's rejection (none accepted)
    #[error("transaction rejected: {0}")]
    Rejected(SendRawTransactionError),

    /// Neither a rejection nor an acceptance: `attempted` entries, the last failure
    #[error("no trusted validator accepted the transaction; {attempted} attempted, last failure: {cause}")]
    Unreachable { attempted: usize, cause: NonDomainError },
}
