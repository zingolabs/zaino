//! Query: fetch transparent address deltas over a height range.

use std::future::Future;

use zaino_primitives::types::{AddressDelta, Height};

use super::QueryError;

/// Domain error for [`GetAddressDeltas`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressDeltasError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

/// Fetch balance deltas for transparent addresses over a height range.
///
/// Maps to `getaddressdeltas` over JSON-RPC.
pub trait GetAddressDeltas: Send + Sync {
    /// Fetch address deltas.
    fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<Output = Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>>> + Send;
}
