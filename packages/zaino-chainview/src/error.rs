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
/// - [`Unavailable`](Self::Unavailable) — statement about the *node*, so its poller stops rather
///   than backing off (`GetMempoolTxidsError::Unavailable`)
/// - [`Source`](Self::Source) — transport, so the poller backs off and retries
///
/// `GetRawMempoolTransactionError::NotFound` is neither: the listing/fetch race is normal, and
/// the poller skips that txid.
#[derive(Debug, thiserror::Error)]
pub enum EndpointPollError {
    #[error("validator exposes no mempool")]
    Unavailable,

    #[error(transparent)]
    Source(#[from] NonDomainError),
}

/// Fewer than [`Quorum::threshold`](crate::Quorum::threshold) endpoints agree on a tip.
///
/// Fail closed: no answer rather than a weak one. Maps to gRPC `UNAVAILABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{agreeing} of {configured} validators agree on a tip; {threshold} required")]
pub struct BelowQuorum {
    pub agreeing: usize,
    pub threshold: usize,
    pub configured: usize,
}

/// No endpoint accepted a relayed transaction.
///
/// A *mixed* result is not an error: a node rejecting what another accepted usually means a
/// stricter local policy (a fee filter), not an invalid transaction.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastError {
    /// Unanimous domain rejection — the real one.
    #[error("every validator rejected the transaction: {0}")]
    Rejected(SendRawTransactionError),

    #[error("no validator accepted the transaction; {attempted} attempted, last failure: {cause}")]
    Unreachable { attempted: usize, cause: NonDomainError },
}
