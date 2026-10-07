//! Port errors: the validator's answer vs. no answer

use core::fmt;

/// Machine-readable no-answer class (decisions match on this, never on messages)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureMode {
    Connection,
    Timeout,
    HttpStatus(u16),
    RpcError(i64),
    Parse,
    Auth,
}

/// No answer from the validator
///
/// - `source()` = concrete cause when one exists (boxed, never stringified)
/// - `message` = the note otherwise (coded refusal)
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

/// `Domain` = the validator's answer; `NonDomain` = none
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: fmt::Debug + fmt::Display> {
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

    /// - Wrapped cause reachable through `QueryError`
    /// - Coded refusal: no source, displays its message
    #[test]
    fn causes_survive_wrapping_and_a_coded_refusal_displays_its_message() {
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
    }
}
