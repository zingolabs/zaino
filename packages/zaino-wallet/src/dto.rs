//! Stable wallet-facing DTOs.
//!
//! The inner surface's domain types are converted to these here, at the adapter,
//! so the inner primitives can evolve without breaking the embedded wallet. The
//! conversions are one-directional (`from_domain`) and named for direction.

use zaino_core::{BlockId, TransactionId};

/// The tip a pinned view is coherent as of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalletTip {
    pub height: u32,
    pub hash: [u8; 32],
}

impl WalletTip {
    pub(crate) fn from_domain(id: BlockId) -> Self {
        Self {
            height: id.height.into(),
            hash: id.hash.into(),
        }
    }
}

/// A transaction identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalletTxId(pub [u8; 32]);

impl WalletTxId {
    pub(crate) fn from_domain(id: TransactionId) -> Self {
        Self(id.into())
    }
}
