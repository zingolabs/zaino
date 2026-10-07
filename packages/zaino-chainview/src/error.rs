//! Why a poll, a query or a broadcast could not complete

use zaino_source::{NonDomainError, SendRawTransactionError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no validator endpoints configured")]
    NoEndpoints,

    #[error("{count} endpoints configured, {max} is the ceiling", max = crate::EndpointSet::MAX)]
    TooManyEndpoints { count: usize },
}

/// One endpoint's failed poll, split on retryability
///
/// - `Unavailable` = about the node: `Down` at once
/// - `Source` = transport or garbled answer: backoff, `Down` at the failure ceiling
/// - neither: `GetRawMempoolTransactionError::NotFound` (normal listing/fetch race, txid skipped)
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

/// Submission no trusted validator accepted or listed (§6)
///
/// - one rejecting, another accepting = success (a refusal = usually local policy)
/// - `Rejected` = precheck's refusal or the first trusted rejection; `Unreachable` = neither
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("transaction rejected: {0}")]
    Rejected(SendRawTransactionError),

    #[error("no trusted validator accepted the transaction; {attempted} attempted, last failure: {cause}")]
    Unreachable { attempted: usize, cause: NonDomainError },
}
