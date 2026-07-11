//! Error types shared across all query traits.

use core::fmt;

/// Transport-level failure (connection refused, timeout, deserialization).
///
/// Shared across all query traits. Domain-specific errors (block not found,
/// height out of range) are per-trait; transport errors are uniform because
/// they depend on the adapter, not the question.
#[derive(Debug, thiserror::Error)]
#[error("transport error: {message}")]
pub struct TransportError {
    message: String,
}

impl TransportError {
    /// Wrap an arbitrary transport failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Combined domain + transport error for a query.
///
/// Generic over the domain error `E`. Each query trait defines its own
/// domain error; this wrapper adds the transport layer uniformly.
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: fmt::Debug + fmt::Display> {
    /// The question has a domain-level answer: "not found", "not ready", etc.
    #[error("{0}")]
    Domain(E),
    /// The question couldn't be delivered or the response couldn't be parsed.
    #[error("{0}")]
    Transport(#[from] TransportError),
}
