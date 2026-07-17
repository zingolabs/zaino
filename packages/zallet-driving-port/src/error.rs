//! Error types shared across all port capabilities.

use core::fmt;

/// How a backend failure relates to retry.
///
/// Machine-readable — drivers match on this to decide whether to retry,
/// never on message strings. Zallet's sync loop retries transient
/// failures (a snapshot take that races the engine's view swap) and
/// surfaces fatal ones. Reads through a snapshot never race a reorg —
/// pinning is unconditional — so the reorg window touches only the
/// unpinned surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Likely to succeed on retry: the failure is a race with chain
    /// movement (the reorg window) or a momentary backend condition.
    Transient,
    /// Retry will not help: the backend is misconfigured, shutting
    /// down, or the request can never succeed.
    Fatal,
}

/// A backend failure crossing the port, classified for retry.
///
/// The driving port abstracts over engines, so no transport detail
/// (HTTP status, JSON-RPC error code) crosses the boundary — only the
/// retry classification and a human-readable description for logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    /// Whether retrying the same call can help.
    pub class: FailureClass,
    /// Human-readable description for logs.
    pub message: String,
}

impl BackendError {
    /// Construct a transient (retryable) backend failure.
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Transient,
            message: message.into(),
        }
    }

    /// Construct a fatal (non-retryable) backend failure.
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Fatal,
            message: message.into(),
        }
    }
}

/// Error from a single port capability.
///
/// Two variants: the capability answered with a domain-level rejection
/// specific to the question asked, or the backend failed before the
/// capability could answer.
#[derive(Debug, thiserror::Error)]
pub enum PortError<E: fmt::Debug + fmt::Display> {
    /// The capability answered with a domain-level rejection.
    #[error("{0}")]
    Domain(E),

    /// The backend failed before the capability could answer.
    #[error("{0}")]
    Backend(BackendError),
}

impl<E: fmt::Debug + fmt::Display> PortError<E> {
    /// Whether retrying the same call can help.
    ///
    /// Domain rejections are answers, not failures, so they are never
    /// transient.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Backend(BackendError {
                class: FailureClass::Transient,
                ..
            })
        )
    }
}

impl<E: fmt::Debug + fmt::Display> From<BackendError> for PortError<E> {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in domain error for exercising the generic wrapper.
    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    #[error("not found")]
    struct NotFound;

    #[test]
    fn transient_backend_failure_is_transient() {
        let err: PortError<NotFound> = BackendError::transient("reorg window").into();
        assert!(err.is_transient());
    }

    #[test]
    fn fatal_backend_failure_is_not_transient() {
        let err: PortError<NotFound> = BackendError::fatal("shutting down").into();
        assert!(!err.is_transient());
    }

    #[test]
    fn domain_rejection_is_not_transient() {
        let err = PortError::Domain(NotFound);
        assert!(!err.is_transient());
    }

    #[test]
    fn display_carries_the_message() {
        let err: PortError<NotFound> = BackendError::transient("validator restarting").into();
        assert_eq!(format!("{err}"), "validator restarting");
    }
}
