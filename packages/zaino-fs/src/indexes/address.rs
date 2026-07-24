//! Addon index: transparent-address history (privacy-sensitive).

use std::future::Future;

use zaino_core::{AddressBalance, TransparentAddress, Utxo};

use crate::error::AddressReadError;

/// Transparent-address history: balance and unspent outputs for an address.
/// Privacy-sensitive — belongs behind a **non-default** feature flag; a
/// deployment that doesn't run it either omits this impl (type-level absence) or
/// returns `NotEnabled` (runtime toggle).
pub trait AddressIndex: Send + Sync {
    fn address_balance(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;
    fn address_unspent(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;
}
