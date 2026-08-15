//! Query: fetch a transaction by txid.

use std::future::Future;

use zaino_primitives::types::{TransactionId, TransactionLocation};

use super::QueryError;

/// A fetched transaction: raw bytes and where it was found.
#[derive(Debug, Clone)]
pub struct TransactionResponse {
    /// Raw serialized transaction bytes.
    pub bytes: Vec<u8>,
    /// Where the transaction was found.
    pub location: TransactionLocation,
}

/// Domain error for [`GetTransaction`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTransactionError {
    /// No transaction with this txid exists.
    #[error("transaction not found: {0}")]
    NotFound(TransactionId),
}

/// Fetch a transaction by its txid.
///
/// Maps to `getrawtransaction(txid, 1)` over JSON-RPC, or the
/// equivalent ReadState query.
pub trait GetTransaction: Send + Sync {
    /// Fetch transaction.
    fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<TransactionResponse, QueryError<GetTransactionError>>> + Send;
}
