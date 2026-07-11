//! Error types shared across all query traits.

use core::fmt;

/// What kind of transport failure occurred.
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
}

/// Transport-level failure from a single attempt.
///
/// Carries a structured [`TransportFailure`] for machine classification
/// and a human-readable message for logging.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    /// What kind of failure.
    pub kind: FailureMode,
    /// Human-readable description.
    pub message: String,
}

impl TransportError {
    /// Construct a transport error.
    pub fn new(kind: FailureMode, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
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
    #[error("{0}")]
    Fetch(TransportError),
}

impl<E: fmt::Debug + fmt::Display> From<TransportError> for QueryError<E> {
    fn from(e: TransportError) -> Self {
        Self::Fetch(e)
    }
}

/// Retries exhausted while trying to reach the validator.
#[derive(Debug, thiserror::Error)]
#[error("unavailable after {attempts} attempts: {last_error}")]
pub struct UnavailableError {
    /// Number of attempts made.
    pub attempts: u32,
    /// The last transport error before giving up.
    pub last_error: TransportError,
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
    #[error("{0}")]
    Fetch(TransportError),

    /// Retries exhausted — the validator is unreachable.
    #[error("{0}")]
    Unavailable(UnavailableError),
}
