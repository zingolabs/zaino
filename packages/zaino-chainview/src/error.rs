//! Why the view could not be built or a broadcast failed

use zaino_source::{NonDomainError, SendRawTransactionError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no validator endpoints configured")]
    NoEndpoints,

    #[error("{count} endpoints configured, {max} is the ceiling", max = zaino_traffic::ValidatorId::MAX)]
    TooManyEndpoints { count: usize },
}

/// Submission no trusted validator accepted or listed (§6)
///
/// - one rejecting, another accepting = success (a refusal = usually local policy)
/// - `Rejected` = precheck's refusal or the first trusted rejection; `Unreachable` = neither
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("transaction rejected: {0}")]
    Rejected(SendRawTransactionError),

    #[error("no trusted validator accepted the transaction; {attempted} attempted, last failure: {cause}")]
    Unreachable { attempted: usize, cause: NonDomainError },
}
