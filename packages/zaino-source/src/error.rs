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
/// [`mode`](Self::mode) is the machine-readable classification the resilience
/// wrapper matches on — never message strings. The concrete adapter cause (a
/// `reqwest`/`jsonrpsee` error, a `serde` failure, a state-service error) is kept
/// as the [`Error::source`](std::error::Error::source) when the failure arose
/// from an error *value*, so logs and diagnostics keep the full chain. A single
/// `FetchError` cannot name every adapter's error type, so at this one forced
/// seam the cause is boxed, never stringified.
///
/// [`message`](Self::message) is a human note for the case ADR-0020 allows a
/// message: when there is **no** underlying error value — a validator's coded
/// refusal (`RpcError` mode) or a self-detected condition. It is empty when a
/// typed cause is present.
#[derive(Debug)]
pub struct FetchError {
    /// What kind of failure.
    pub mode: FailureMode,
    /// Human note for the no-error-value case; empty when [`source`] carries the
    /// cause.
    ///
    /// [`source`]: std::error::Error::source
    pub message: String,
    /// The concrete cause, when the failure came from an error value.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl FetchError {
    /// A failure with **no** underlying error value: a coded refusal or a
    /// self-detected condition. `message` is the human note.
    pub fn new(mode: FailureMode, message: impl Into<String>) -> Self {
        Self {
            mode,
            message: message.into(),
            source: None,
        }
    }

    /// A failure that **wraps a concrete cause**, preserved as the source chain.
    /// Use this instead of `new(mode, e.to_string())` whenever `e` is an error
    /// value — it keeps the cause's type and `source()` chain.
    pub fn from_cause(
        mode: FailureMode,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            mode,
            message: String::new(),
            source: Some(cause.into()),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.message.is_empty() {
            write!(f, "{}", self.message)
        } else if let Some(cause) = &self.source {
            write!(f, "{cause}")
        } else {
            write!(f, "{:?}", self.mode)
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|cause| cause.as_ref() as &(dyn std::error::Error + 'static))
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

    /// Transport-level failure — Display and `source()` delegate to the
    /// [`FetchError`], so an abort trail reaches the concrete transport cause.
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
#[error("unavailable after {attempts} attempts: {last_error}")]
pub struct UnavailableError {
    /// Number of attempts made.
    pub attempts: u32,
    /// The last transport error before giving up — kept as the source so the
    /// chain reaches the concrete transport cause.
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
    #[error(transparent)]
    Fetch(FetchError),

    /// Retries exhausted — the validator is unreachable.
    #[error(transparent)]
    Unavailable(UnavailableError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[derive(Debug, thiserror::Error)]
    #[error("concrete transport cause")]
    struct Cause;

    /// Walk `source()` looking for the concrete cause.
    fn chain_reaches_cause(top: &(dyn Error + 'static)) -> bool {
        let mut cursor = Some(top);
        while let Some(err) = cursor {
            if err.downcast_ref::<Cause>().is_some() {
                return true;
            }
            cursor = err.source();
        }
        false
    }

    #[test]
    fn from_cause_survives_the_wrappers_as_a_source_chain() {
        // The transport error keeps the concrete cause reachable...
        let fetch = FetchError::from_cause(FailureMode::Parse, Cause);
        assert!(chain_reaches_cause(&fetch));

        // ...and it still reaches it through the consumer-facing wrappers, both
        // the transparent Fetch variant and the Unavailable summary.
        let via_fetch: SourceError<String> =
            SourceError::Fetch(FetchError::from_cause(FailureMode::Parse, Cause));
        assert!(chain_reaches_cause(&via_fetch));

        let via_unavailable: SourceError<String> = SourceError::Unavailable(UnavailableError {
            attempts: 3,
            last_error: FetchError::from_cause(FailureMode::Connection, Cause),
        });
        assert!(chain_reaches_cause(&via_unavailable));
    }

    #[test]
    fn new_is_the_no_error_value_case() {
        // A coded refusal carries a message and no source — the ADR-permitted
        // "no underlying error value" case the serve layer reads back out.
        let refusal = FetchError::new(FailureMode::RpcError(-8), "rejected");
        assert!(refusal.source().is_none());
        assert_eq!(refusal.message, "rejected");
        assert_eq!(refusal.to_string(), "rejected");
    }
}
