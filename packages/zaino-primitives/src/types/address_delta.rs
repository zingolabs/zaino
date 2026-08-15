//! Transparent address balance delta.

use super::{Height, OutputIndex, SignedZatoshis, TransactionId, TransparentAddress};

/// A single balance change for a transparent address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDelta {
    /// Change in zatoshis (negative for spends, positive for receives).
    pub satoshis: SignedZatoshis,
    /// The transaction that caused this delta.
    pub txid: TransactionId,
    /// Input or output index within the transaction.
    pub index: OutputIndex,
    /// Block height where this delta occurred.
    pub height: Height,
    /// The transparent address affected.
    pub address: TransparentAddress,
    /// Zero-based position of the transaction within its containing block.
    ///
    /// `None` when the source cannot supply it. zcashd documents
    /// `getaddressdeltas` as ordered by `(height, blockindex, index)`, so a
    /// source that knows this can be sorted correctly and one that does not
    /// cannot — modelling it as optional keeps that distinction visible instead
    /// of silently substituting iteration order.
    pub block_index: Option<u32>,
}
