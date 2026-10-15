//! Why the view could not be built or a broadcast failed

use zaino_source::{NonDomainError, SendRawTransactionError};

/// Empty or over-[`TrustedValidatorId::MAX`](zaino_traffic::TrustedValidatorId) lists included
/// (the balancer refuses both, so neither matches its members)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{addresses} trusted validator addresses for {trusted} balancer members")]
    AddressCountMismatch { addresses: usize, trusted: usize },
}

/// Submission no trusted validator accepted or listed
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
