//! Recent facet: transparent-address unspent outputs within the window.

use zaino_core::{TransparentAddress, Utxo};

/// Unspent outpoints for `addr` created within (and still unspent within) this
/// window — re-derived from the window's blocks, infallible. Merged with the FS
/// address index by the runtime for a snapshot-coherent unspent set (US-1.3).
pub trait NfsAddressFacts: Send + Sync {
    fn address_unspent(&self, addr: &TransparentAddress) -> Vec<Utxo>;
}
