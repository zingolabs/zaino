//! Transparent address balance delta.

use super::{Height, OutputIndex, SignedZatoshis, TransactionHash, TransparentAddress};

/// A single balance change for a transparent address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDelta {
    /// Change in zatoshis (negative for spends, positive for receives).
    pub satoshis: SignedZatoshis,
    /// The transaction that caused this delta.
    pub txid: TransactionHash,
    /// Input or output index within the transaction.
    pub index: OutputIndex,
    /// Block height where this delta occurred.
    pub height: Height,
    /// The transparent address affected.
    pub address: TransparentAddress,
}
