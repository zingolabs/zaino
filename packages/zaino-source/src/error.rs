//! Port errors: the validator's answer vs. no answer

use core::fmt;

/// Machine-readable no-answer class (retry decisions match on this, never on messages)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureMode {
    Connection,
    Timeout,
    HttpStatus(u16),
    RpcError(i64),
    Parse,
    Auth,
}

impl FailureMode {
    /// Asking again can change the outcome
    ///
    /// - `-1` work queue full, `-28` warming up (every other code = the node's considered reply)
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Connection | Self::Timeout => true,
            Self::HttpStatus(code) => *code >= 500,
            Self::RpcError(code) => matches!(code, -1 | -28),
            Self::Parse | Self::Auth => false,
        }
    }
}

/// The validator did not answer the question
///
/// - `source()` = the concrete cause when there is an error value (boxed, never stringified)
/// - `message` = the note when there is none (a coded refusal)
#[derive(Debug)]
pub struct NonDomainError {
    pub mode: FailureMode,
    pub message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl NonDomainError {
    pub fn new(mode: FailureMode, message: impl Into<String>) -> Self {
        Self { mode, message: message.into(), source: None }
    }

    pub fn from_cause(
        mode: FailureMode,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self { mode, message: String::new(), source: Some(cause.into()) }
    }
}

impl fmt::Display for NonDomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            _ if !self.message.is_empty() => write!(f, "{}", self.message),
            Some(cause) => write!(f, "{cause}"),
            None => write!(f, "{:?}", self.mode),
        }
    }
}

impl std::error::Error for NonDomainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|cause| cause.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: fmt::Debug + fmt::Display> {
    /// The validator's answer
    #[error("{0}")]
    Domain(E),
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

    /// A wrapped cause stays reachable through `QueryError`; a coded refusal has no source and
    /// displays its message; only no-answer classes asking again can change are transient
    #[test]
    fn causes_survive_wrapping_and_only_transient_modes_retry() {
        let wrapped: QueryError<String> =
            NonDomainError::from_cause(FailureMode::Parse, Cause).into();
        let mut cursor: Option<&(dyn Error + 'static)> = Some(&wrapped);
        let mut reached = false;
        while let Some(err) = cursor {
            reached |= err.downcast_ref::<Cause>().is_some();
            cursor = err.source();
        }
        assert!(reached, "cause lost in {wrapped:?}");

        let refusal = NonDomainError::new(FailureMode::RpcError(-8), "rejected");
        assert!(refusal.source().is_none());
        assert_eq!(refusal.to_string(), "rejected");

        for (mode, transient) in [
            (FailureMode::Connection, true),
            (FailureMode::Timeout, true),
            (FailureMode::HttpStatus(503), true),
            (FailureMode::HttpStatus(404), false),
            (FailureMode::RpcError(-1), true),
            (FailureMode::RpcError(-28), true),
            (FailureMode::RpcError(-8), false),
            (FailureMode::Parse, false),
            (FailureMode::Auth, false),
        ] {
            assert_eq!(mode.is_transient(), transient, "{mode:?}");
        }
    }
}
