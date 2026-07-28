//! Query: fetch transparent address balance.

use std::future::Future;

use zaino_primitives::types::AddressBalance;

use super::QueryError;

/// Domain error for [`GetAddressBalance`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressBalanceError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

/// Fetch the balance of one or more transparent addresses.
///
/// Maps to `getaddressbalance` over JSON-RPC.
pub trait GetAddressBalance: Send + Sync {
    /// Fetch address balance.
    fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<Output = Result<AddressBalance, QueryError<GetAddressBalanceError>>> + Send;
}
