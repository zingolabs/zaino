//! Hold error types for the queue and related functionality.

use crate::nym::error::NymError;

/// Zingo-Indexer request errors.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Errors originating from incorrect enum types being called.
    #[error("Incorrect variant")]
    IncorrectVariant,
    /// System time errors.
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    /// Nym Related Errors
    #[error("Nym error: {0}")]
    NymError(#[from] NymError),
}
