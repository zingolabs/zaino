//! Statistics over the whole unspent transparent output set.

use crate::types::{BlockHash, Height, Zatoshis};

/// Statistics describing the transparent UTXO set at a given chain tip.
///
/// A domain type rather than one of the proxied [`rpc`](super::rpc) shapes:
/// Zaino answers `gettxoutsetinfo` from its own finalised-state accumulator,
/// not by asking the validator, so there is no source port for it and no
/// validator response to forward.
///
/// Computing these statistics requires a full pass over the UTXO set, so the
/// answer may be unavailable — that is reported as no result rather than as a
/// variant here, so this type always describes a real, complete measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOutSetInfo {
    /// Height the statistics were computed at.
    pub height: Height,

    /// Best-chain block the statistics were computed against.
    pub best_block: BlockHash,

    /// Number of transactions holding at least one unspent transparent output.
    pub transactions: u64,

    /// Number of unspent transparent outputs.
    pub tx_outs: u64,

    /// Serialised size of the UTXO set, in bytes.
    pub bytes_serialized: u64,

    /// Hash over the serialised UTXO set.
    ///
    /// A `String` rather than a hash newtype: its width and construction are
    /// validator-defined, not a protocol-fixed digest, so Zaino forwards it
    /// without claiming to know its shape.
    pub hash_serialized: String,

    /// Total value held across every unspent transparent output.
    ///
    /// The wire form is ZEC-denominated; the adapter converts to integer
    /// zatoshis. This is a chain-supply-scale figure, so a float would lose
    /// precision outright.
    pub total_amount: Zatoshis,
}
