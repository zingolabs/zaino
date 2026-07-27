//! Query: fetch unspent transparent outputs for addresses.

use std::future::Future;

use zaino_primitives::types::Utxo;

use super::QueryError;

/// Domain error for [`GetAddressUtxos`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressUtxosError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

/// Fetch unspent transparent outputs for one or more addresses.
///
/// Maps to `getaddressutxos` over JSON-RPC.
pub trait GetAddressUtxos: Send + Sync {
    /// Fetch UTXOs.
    fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Utxo>, QueryError<GetAddressUtxosError>>> + Send;
}
