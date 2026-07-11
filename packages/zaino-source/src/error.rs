//! Transport-level error shared across all query traits.

use core::fmt;

/// Transport-level failure (connection refused, timeout, deserialization).
///
/// Shared across all query traits. Domain-specific errors (block not found,
/// height out of range) are per-trait; transport errors are uniform because
/// they depend on the adapter, not the question.
#[derive(Debug)]
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

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "transport error: {}", self.message)
    }
}

impl std::error::Error for TransportError {}
