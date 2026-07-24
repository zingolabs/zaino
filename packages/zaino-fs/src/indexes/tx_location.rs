//! Addon index: transaction location by txid.

use std::future::Future;

use zaino_core::{TransactionHash, TransactionLocation};

use crate::error::LookupError;

/// Resolve a txid to where it was mined. Backed by the txid→location index.
/// A miss is `Ok(None)`; only the backend can fail.
pub trait TxLocationIndex: Send + Sync {
    fn tx_location(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<TransactionLocation>, LookupError>> + Send;
}
