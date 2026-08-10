//! Transparent address balance.

use super::Zatoshis;

/// Balance information for a set of transparent addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBalance {
    /// Total current balance in zatoshis.
    pub balance: Zatoshis,
    /// Total received in zatoshis (lifetime).
    pub received: Zatoshis,
}
