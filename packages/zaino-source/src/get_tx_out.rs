//! Query: fetch an unspent transparent output.

use std::future::Future;

use zaino_primitives::types::{rpc::TxOut, OutputIndex, TransactionId};

use super::QueryError;

/// Domain error for [`GetTxOut`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTxOutError {
    /// The referenced transaction is not known to the validator.
    #[error("no transaction {0}")]
    TransactionNotFound(TransactionId),
}

/// Fetch an unspent transparent output by outpoint.
///
/// `Ok(None)` means the outpoint is *spent or nonexistent* — the ordinary
/// answer to "is this unspent?", not a failure. A missing transaction is an
/// error because the question could not be evaluated at all.
///
/// Maps to `gettxout` over JSON-RPC.
pub trait GetTxOut: Send + Sync {
    /// Fetch an unspent output.
    ///
    /// `include_mempool` asks the validator to account for unconfirmed spends,
    /// so an output spent only in the mempool reports as absent.
    fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> impl Future<Output = Result<Option<TxOut>, QueryError<GetTxOutError>>> + Send;
}
