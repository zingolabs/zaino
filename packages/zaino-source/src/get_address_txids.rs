//! Query: fetch transaction ids for transparent addresses.

use std::future::Future;

use zaino_primitives::types::{Height, TransactionHash};

use super::QueryError;

/// Domain error for [`GetAddressTxids`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressTxidsError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

/// Fetch transaction ids involving transparent addresses over a height range.
///
/// Maps to `getaddresstxids` over JSON-RPC.
pub trait GetAddressTxids: Send + Sync {
    /// Fetch address txids.
    fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<Output = Result<Vec<TransactionHash>, QueryError<GetAddressTxidsError>>>
           + Send;
}
