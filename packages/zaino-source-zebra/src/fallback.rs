//! When a state-path miss should be retried over the fetch path.

use zaino_source::QueryError;

/// Whether a state-service answer of "no" should be retried over JSON-RPC.
///
/// Only a *domain* miss is retried. The state service answering `NotFound` can
/// mean two different things — the block genuinely does not exist, or it exists
/// somewhere the finalized state does not reach (a side chain, the mempool) —
/// and only JSON-RPC can tell them apart.
///
/// A transport failure is deliberately **not** retried. Falling back there
/// would turn a broken or misconfigured state database into a silent, permanent
/// performance collapse: every query would still succeed, over the fetch path,
/// with nothing surfacing the fault. An operator would see latency and no
/// error. Failing loudly is the more useful behaviour.
pub(crate) fn retry_over_fetch<T, E>(state: &Result<T, QueryError<E>>) -> bool
where
    E: std::fmt::Debug + std::fmt::Display,
{
    matches!(state, Err(QueryError::Domain(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::Height;
    use zaino_source::{FailureMode, GetBlockError, NonDomainError};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid height")
    }

    #[test]
    fn a_domain_miss_is_retried() {
        let miss: Result<(), _> = Err(QueryError::Domain(GetBlockError::HeightNotFound(height(1))));

        assert!(retry_over_fetch(&miss));
    }

    /// The case that matters: a broken state database must surface as an error
    /// rather than quietly routing every query over the fetch path forever.
    #[test]
    fn a_transport_failure_is_not_retried() {
        let broken: Result<(), QueryError<GetBlockError>> = Err(QueryError::NonDomain(
            NonDomainError::new(FailureMode::Connection, "database unavailable"),
        ));

        assert!(!retry_over_fetch(&broken));
    }

    #[test]
    fn success_is_not_retried() {
        let found: Result<(), QueryError<GetBlockError>> = Ok(());

        assert!(!retry_over_fetch(&found));
    }
}
