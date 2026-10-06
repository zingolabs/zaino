//! Query: fetch the validator's recommended fee per logical action.

use std::future::Future;

use zaino_primitives::types::rpc::StandardFee;

use super::QueryError;

/// Domain error for [`GetStandardFee`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetStandardFeeError {
    /// The validator is not ready to report a fee.
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the fee per logical action wallets should pay for a transaction
/// mined in the next block.
///
/// Maps to `getstandardfee` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetStandardFee: Send + Sync {
    /// Fetch the recommended fee.
    fn get_standard_fee(
        &self,
    ) -> impl Future<Output = Result<StandardFee, QueryError<GetStandardFeeError>>> + Send;
}
