//! Why a broadcast failed

use zaino_source::{NonDomainError, SendRawTransactionError};

/// Submission no trusted validator accepted or listed
///
/// - one rejecting, another accepting = success (a refusal = usually local policy)
/// - `Rejected` = precheck's refusal or the first trusted rejection
/// - `Unreachable` = neither
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("transaction rejected: {0}")]
    Rejected(SendRawTransactionError),

    #[error("no trusted validator accepted the transaction; {attempted} attempted, last failure: {cause}")]
    Unreachable { attempted: usize, cause: NonDomainError },
}
