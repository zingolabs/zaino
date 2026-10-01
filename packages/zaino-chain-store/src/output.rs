//! Transparent outputs, as the store indexes them.

use zaino_primitives::types::{BlockTxPosition, TransactionId, TransparentAddressKey, Zatoshis};

/// A transparent output, as the store holds it.
///
/// Carries what an index needs — how much, and to whom — not the output
/// itself. The locking script is not stored, so this cannot reproduce the
/// bytes a block committed to. A consumer needing those asks the validator for
/// the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredTxOut {
    /// The output's value.
    pub value: Zatoshis,
    /// The address it pays to, as an index keys it.
    ///
    /// [`TransparentAddressKey`] rather than a wallet-facing address type: the
    /// store indexes outputs that have no address at all, and one that dropped
    /// them would answer "no history" for an address that has some.
    pub address: TransparentAddressKey,
}

impl StoredTxOut {
    /// An output from its parts.
    pub fn new(value: Zatoshis, address: TransparentAddressKey) -> Self {
        Self { value, address }
    }
}

/// The transaction that spent an outpoint, and where it sits.
///
/// Carries the txid alongside the position because every caller wants it: a
/// spend is reported to a client as "spent by this transaction", and a
/// position alone requires a second lookup to become that. Resolving it once,
/// where the index is already open, is cheaper than making every consumer do
/// it — and on a batched query it is the difference between one round trip and
/// one per spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpenderRef {
    /// Where the spending transaction sits.
    pub position: BlockTxPosition,
    /// Which transaction it is.
    pub txid: TransactionId,
}

impl SpenderRef {
    /// A spender reference from its parts.
    pub fn new(position: BlockTxPosition, txid: TransactionId) -> Self {
        Self { position, txid }
    }
}
