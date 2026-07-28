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
