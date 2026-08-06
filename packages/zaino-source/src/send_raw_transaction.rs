//! Command: submit a transaction to the network.

use std::future::Future;

use zaino_primitives::types::TransactionId;

use super::QueryError;

/// Domain error for [`SendRawTransaction`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SendRawTransactionError {
    /// The transaction could not be deserialised.
    #[error("malformed transaction: {0}")]
    Malformed(String),

    /// The validator rejected the transaction — invalid, or it failed policy.
    #[error("rejected by validator: {0}")]
    Rejected(String),
}

/// Submit a serialised transaction to the validator's mempool for relay.
///
/// The only mutating operation in this crate. It is not idempotent in the way
/// the queries are: resubmitting a transaction already in the mempool may
/// succeed or be rejected as a duplicate depending on the validator, so callers
/// must not treat an error as proof the transaction was not accepted earlier.
///
/// Maps to `sendrawtransaction` over JSON-RPC.
pub trait SendRawTransaction: Send + Sync {
    /// Submit a serialised transaction, returning its id on acceptance.
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<Output = Result<TransactionId, QueryError<SendRawTransactionError>>> + Send;
}
