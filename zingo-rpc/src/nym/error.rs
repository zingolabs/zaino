//! Hold error types for Nym related functionality.

use crate::blockcache::error::ParseError;

/// Parser Error Type.
#[derive(Debug, thiserror::Error)]
pub enum NymError {
    /// Serialization and deserialization error.
    #[error("Parser Error: {0}")]
    ParseError(#[from] ParseError),
    /// Nym-SDK related error, look at specific types for detail.
    ///
    /// TODO: Handle certain Nym-SDK Errors specifically (network related errors, nym client startup..).
    #[error("Nym-SDK Error: {0}")]
    NymError(#[from] nym_sdk::Error),
    /// Nym address formatting errors.
    #[error("Nym Recipient Formatting Error: {0}")]
    RecipientFormattingError(#[from] nym_sphinx_addressing::clients::RecipientFormattingError),
    /// Mixnet connection error.
    #[error("Connection Error: {0}")]
    ConnectionError(String),
    /// Custom error for empty messages received from the Nym network.
    #[error("Empty message received from the mixnet")]
    EmptyMessageError,
    /// Custom error for receiveing not AnonSenderTag (surb) from the Nym network.
    #[error("No AnonSenderTag received from the mixnet")]
    EmptyRecipientTagError,
}

impl From<NymError> for tonic::Status {
    fn from(error: NymError) -> Self {
        match error {
            NymError::ParseError(e) => tonic::Status::internal(format!("Parse error: {}", e)),
            NymError::NymError(e) => tonic::Status::internal(format!("Nym-SDK error: {}", e)),
            NymError::RecipientFormattingError(e) => {
                tonic::Status::invalid_argument(format!("Recipient formatting error: {}", e))
            }
            NymError::ConnectionError(e) => {
                tonic::Status::internal(format!("Connection error: {}", e))
            }
            NymError::EmptyMessageError => {
                tonic::Status::internal(format!("Empty message received from nym mixnet"))
            }
            NymError::EmptyRecipientTagError => {
                tonic::Status::internal(format!("No AnonSenderTag received from nym mixnet"))
            }
        }
    }
}
