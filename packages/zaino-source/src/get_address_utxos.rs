//! Query: fetch unspent transparent outputs for addresses.

use std::future::Future;

use zaino_primitives::types::Utxo;

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetAddressUtxos`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressUtxosError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),

    /// More outputs match than the validator answers in one query.
    #[error("more than {limit} unspent outputs match; page with startHeight and maxEntries")]
    TooManyResults {
        /// The cap that was exceeded.
        limit: usize,
    },
}

/// Fetch unspent transparent outputs for one or more addresses.
///
/// Maps to `getaddressutxos` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetAddressUtxos: ValidatorSource + Send + Sync {
    /// Fetch UTXOs.
    fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Utxo>, QueryError<GetAddressUtxosError, Self::NonDomain>>> + Send;
}
