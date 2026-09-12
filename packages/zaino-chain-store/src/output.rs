//! Transparent outputs, as the store indexes them.

use zaino_primitives::types::{BlockTxPosition, ScriptType, TransactionId, Zatoshis};

/// A transparent output's address, as the store keys it.
///
/// A 20-byte hash and the script form it came from — **not** a recoverable
/// script. For P2PKH the hash is a public-key hash and for P2SH a script hash,
/// so the pair is enough to reconstruct a standard address. For
/// [`ScriptType::NonStandard`] it is not: there is no address, and what the 20
/// bytes hold is whatever the indexing backend chose to key such an output by.
///
/// This is why the store's address surface is not typed on
/// [`TransparentAddress`](zaino_primitives::types::TransparentAddress). That
/// type can only name outputs that *have* an address, and the store
/// deliberately indexes ones that do not — an index that silently dropped
/// non-standard outputs would answer "no history" for an address that has
/// some.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoredAddress {
    /// The 20-byte hash the output is keyed by.
    pub hash: [u8; 20],
    /// Which script form those bytes came from.
    pub script_type: ScriptType,
}

impl StoredAddress {
    /// An address key from its parts.
    pub fn new(hash: [u8; 20], script_type: ScriptType) -> Self {
        Self { hash, script_type }
    }

    /// Whether this key names a standard, reconstructible address.
    ///
    /// `false` means the 20 bytes are an indexing key and nothing more: they
    /// will not round-trip to a script, and they may collide with another
    /// non-standard output.
    pub fn is_standard(&self) -> bool {
        !matches!(self.script_type, ScriptType::NonStandard)
    }
}

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
    /// The address it pays to, as the store keys it.
    pub address: StoredAddress,
}

impl StoredTxOut {
    /// An output from its parts.
    pub fn new(value: Zatoshis, address: StoredAddress) -> Self {
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
