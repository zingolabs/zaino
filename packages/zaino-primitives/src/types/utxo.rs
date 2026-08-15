//! Unspent transparent output.

use super::{Height, OutputIndex, Script, TransactionId, TransparentAddress, Zatoshis};

/// An unspent transparent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    /// The transparent address holding this output.
    pub address: TransparentAddress,
    /// The transaction containing this output.
    pub txid: TransactionId,
    /// Output index within the transaction.
    pub output_index: OutputIndex,
    /// The output script.
    pub script: Script,
    /// Value in zatoshis.
    pub satoshis: Zatoshis,
    /// Block height where this output was created.
    pub height: Height,
}
