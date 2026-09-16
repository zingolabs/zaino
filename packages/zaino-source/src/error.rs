//! Error types shared across all query traits.

use core::fmt;

/// What kind of source failure occurred.
///
/// Machine-readable — the resilience wrapper matches on this to
/// decide retryability, not on message strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureMode {
    /// Connection refused, DNS failure, TLS handshake error.
    Connection,
    /// Request timed out.
    Timeout,
    /// Non-2xx HTTP status code.
    HttpStatus(u16),
    /// Server returned a JSON-RPC error code.
    RpcError(i64),
    /// Response couldn't be deserialized.
    Parse,
    /// Authentication rejected.
    Auth,
    /// The source answered, and the answer violates an invariant the adapter
    /// relies on: a value outside its domain type's range, a response that
    /// does not correspond to the request, or two indexes that disagree.
    ///
    /// Distinct from [`Parse`](Self::Parse): nothing failed to deserialize,
    /// and an in-process source has no wire format to fail on. Not retryable:
    /// the same read returns the same data.
    InvalidSourceData,
}

/// An error that caused a [`FetchError`], kept for the operator's log.
///
/// Boxed because the adapters' underlying error types (a Zebra state-service
/// error, an HTTP client error, a primitive's range error) are not something
/// this crate can name.
pub type BoxCause = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A source failed to answer a single attempt.
///
/// Carries a [`FailureMode`] for machine classification, a human-readable
/// message naming what failed, and, when there is one, the underlying error as
/// [`source`](std::error::Error::source).
///
/// `Display` renders the message only; the cause is reached through the
/// source chain, so a chain printer does not repeat it.
///
/// Not `Clone` or `PartialEq`: the cause is a [`BoxCause`], which is neither.
/// Compare the [`mode`](Self::mode) instead.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FetchError {
    /// What kind of failure.
    pub mode: FailureMode,
    /// Human-readable description of what failed.
    pub message: String,
    /// The error that caused this one, when there is one.
    #[source]
    cause: Option<BoxCause>,
}

impl FetchError {
    /// A failure with no underlying error to hand over.
    pub fn new(mode: FailureMode, message: impl Into<String>) -> Self {
        Self {
            mode,
            message: message.into(),
            cause: None,
        }
    }

    /// A failure caused by `cause`.
    ///
    /// `message` names what failed, not why: the cause's own text is reported
    /// through the source chain.
    pub fn because(
        mode: FailureMode,
        message: impl Into<String>,
        cause: impl Into<BoxCause>,
    ) -> Self {
        Self {
            mode,
            message: message.into(),
            cause: Some(cause.into()),
        }
    }
}

/// Single-attempt error from an adapter.
///
/// Two variants: the server answered with a domain rejection, or the
/// transport failed. No retry awareness.
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: fmt::Debug + fmt::Display> {
    /// The server answered with a domain-level rejection.
    #[error("{0}")]
    Domain(E),

    /// Transport-level failure.
    ///
    /// Transparent, so the [`FetchError`]'s cause stays on the source chain.
    #[error(transparent)]
    Fetch(FetchError),
}

impl<E: fmt::Debug + fmt::Display> From<FetchError> for QueryError<E> {
    fn from(e: FetchError) -> Self {
        Self::Fetch(e)
    }
}

/// Retries exhausted while trying to reach the validator.
#[derive(Debug, thiserror::Error)]
#[error("unavailable after {attempts} attempts")]
pub struct UnavailableError {
    /// Number of attempts made.
    pub attempts: u32,
    /// The last transport error before giving up.
    #[source]
    pub last_error: FetchError,
}

/// Consumer-facing error from the resilience wrapper.
///
/// - `Domain`: the server answered "no" (never retried)
/// - `Transport`: non-retryable transport failure (passed through)
/// - `Unavailable`: retryable failure, retries exhausted
#[derive(Debug, thiserror::Error)]
pub enum SourceError<E: fmt::Debug + fmt::Display> {
    /// The server answered with a domain-level rejection.
    #[error("{0}")]
    Domain(E),

    /// Non-retryable transport failure.
    ///
    /// Transparent, so the [`FetchError`]'s cause stays on the source chain.
    #[error(transparent)]
    Fetch(FetchError),

    /// Retries exhausted — the validator is unreachable.
    ///
    /// Transparent, so the last attempt's error stays on the source chain.
    #[error(transparent)]
    Unavailable(UnavailableError),
}

#[cfg(test)]
mod tests {
    use super::{FailureMode, FetchError};
    use std::error::Error as _;

    /// A stand-in for an adapter's underlying error.
    #[derive(Debug, thiserror::Error)]
    #[error("row 7 is out of range")]
    struct Underlying;

    /// The cause is reachable through the source chain, and the message does
    /// not repeat it.
    #[test]
    fn a_caused_failure_reports_why_once() {
        let error = FetchError::because(
            FailureMode::InvalidSourceData,
            "state service returned an invalid height",
            Underlying,
        );

        assert_eq!(
            error.to_string(),
            "state service returned an invalid height"
        );
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("row 7 is out of range".to_string()),
        );
    }

    /// Wrapping a failure in a port error keeps its cause reachable.
    #[test]
    fn a_query_error_forwards_the_cause() {
        let error: super::QueryError<String> = FetchError::because(
            FailureMode::InvalidSourceData,
            "state service returned an invalid height",
            Underlying,
        )
        .into();

        assert_eq!(
            error.to_string(),
            "state service returned an invalid height"
        );
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("row 7 is out of range".to_string()),
        );
    }

    /// Exhausted retries report the last attempt as the cause rather than
    /// repeating it in the message.
    #[test]
    fn an_unavailable_error_reports_the_last_attempt_as_its_source() {
        let error = super::UnavailableError {
            attempts: 3,
            last_error: FetchError::new(FailureMode::Timeout, "no answer"),
        };

        assert_eq!(error.to_string(), "unavailable after 3 attempts");
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("no answer".to_string()),
        );
    }

    /// A failure with nothing to hand over ends the chain.
    #[test]
    fn an_uncaused_failure_has_no_source() {
        let error = FetchError::new(FailureMode::Timeout, "no answer");

        assert!(error.source().is_none());
    }
}
