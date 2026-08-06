//! Query: fetch transaction ids for transparent addresses.

use std::future::Future;

use zaino_primitives::types::{Height, TransactionId};

use super::QueryError;

/// Domain error for [`GetAddressTxids`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressTxidsError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),

    /// The requested height range cannot be served: it is inverted, or it
    /// reaches above the chain tip.
    ///
    /// A domain rejection rather than a transport failure — the request is
    /// answerable in principle, just not for these bounds, and retrying it
    /// unchanged will fail the same way.
    #[error("unserviceable height range {start}..={end}")]
    InvalidRange {
        /// First height requested.
        start: Height,
        /// Last height requested.
        end: Height,
    },
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
    ) -> impl Future<Output = Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>>> + Send;
}
