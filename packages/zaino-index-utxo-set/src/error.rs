//! [`FoldError`]: a block the utxo set cannot fold, or a held block's fees it cannot read back

use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::TxId;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    /// Prevout not unspent here: never recorded, or spent already (indexed from genesis: a bug
    /// or a foreign directory, never a gap to tolerate)
    #[error(
        "block {height} tx {txid}: spends {spent}:{vout}, not an unspent output of this index"
    )]
    MissingPrevout { height: BlockHeight, txid: TxId, spent: TxId, vout: u32 },

    #[error("block {height} tx {txid}: value sums past the money supply")]
    ValueOverflow { height: BlockHeight, txid: TxId },

    #[error("block {height} tx {txid}: takes more from the transparent pool than it puts in")]
    NegativeFee { height: BlockHeight, txid: TxId },

    #[error("block {height}: held, but no fees record")]
    StoredFeesMissing { height: BlockHeight },

    #[error("block {height}: fees record malformed or not one fee per transaction")]
    StoredFeesUnreadable { height: BlockHeight },
}
