//! Port errors: the validator's answer vs. no answer

use core::fmt;

/// Machine-readable no-answer class: the failure metric's label + answered / unanswered split
/// (no routing decision reads it)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    Connection,
    Timeout,
    HttpStatus(u16),
    RpcError(i64),
    /// Indexer gRPC's status (transport losses = `Connection`)
    GrpcStatus(tonic::Code),
    Parse,
    Auth,
}

type Cause = Box<dyn std::error::Error + Send + Sync + 'static>;

/// No answer from the validator: a concrete cause (`source()`, boxed, never stringified), else a
/// note (coded refusal)
#[derive(Debug)]
pub struct NonDomainError {
    pub mode: FailureMode,
    detail: Detail,
}

#[derive(Debug)]
enum Detail {
    Note(String),
    Cause(Cause),
}

impl NonDomainError {
    pub fn new(mode: FailureMode, note: impl Into<String>) -> Self {
        Self { mode, detail: Detail::Note(note.into()) }
    }

    pub fn from_cause(mode: FailureMode, cause: impl Into<Cause>) -> Self {
        Self { mode, detail: Detail::Cause(cause.into()) }
    }
}

impl fmt::Display for NonDomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Detail::Note(note) => f.write_str(note),
            Detail::Cause(cause) => write!(f, "{cause}"),
        }
    }
}

impl std::error::Error for NonDomainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.detail {
            Detail::Note(_) => None,
            Detail::Cause(cause) => Some(cause.as_ref()),
        }
    }
}

/// Why a query has no value
///
/// - `Domain` = the validator's answer
/// - `Unsupported` = no such method on it (JSON-RPC `-32601`: an answer about the node, never a
///   failure)
/// - `NonDomain` = no answer
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: fmt::Debug + fmt::Display> {
    #[error("{0}")]
    Domain(E),
    #[error("no such method on this validator")]
    Unsupported,
    #[error(transparent)]
    NonDomain(NonDomainError),
}

impl<E: fmt::Debug + fmt::Display> From<NonDomainError> for QueryError<E> {
    fn from(e: NonDomainError) -> Self {
        Self::NonDomain(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[derive(Debug, thiserror::Error)]
    #[error("concrete transport cause")]
    struct Cause;

    #[test]
    fn a_wrapped_cause_stays_reachable_through_query_error() {
        let wrapped: QueryError<String> =
            NonDomainError::from_cause(FailureMode::Parse, Cause).into();

        let mut cursor: Option<&(dyn Error + 'static)> = Some(&wrapped);
        let mut reached = false;
        while let Some(err) = cursor {
            reached |= err.downcast_ref::<Cause>().is_some();
            cursor = err.source();
        }

        assert!(reached, "cause lost in {wrapped:?}");
    }
}
