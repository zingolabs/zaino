//! TxidLocationIndex (BlockLocal × Append): txid → (height, tx_index).

use zaino_persistence_codec::{DecodeError, EntryCodec, FormatVersion};
use zaino_primitives::types::TransactionId;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context.
pub struct TxidLocationCtx {
    /// (txid, height, tx_index) for each transaction.
    pub locations: Vec<(TransactionId, BlockHeight, u32)>,
}

/// Delta: one entry per transaction.
pub struct TxidLocationEntry {
    /// Transaction hash (key).
    pub txid: TransactionId,
    /// Location (value).
    pub location: TxLocation,
}

/// Persisted value: height + tx_index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxLocation {
    /// Block height.
    pub height: BlockHeight,
    /// Transaction index within the block.
    pub tx_index: u32,
}

/// Index definition.
pub struct TxidLocationIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("txid_location");

impl IndexDef for TxidLocationIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<TxidLocationEntry>;
    type BlockContext = TxidLocationCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for TxidLocationIndex {
    fn extract(ctx: &TxidLocationCtx) -> Result<Self::Delta, ExtractError> {
        Ok(ctx
            .locations
            .iter()
            .map(|(txid, height, idx)| TxidLocationEntry {
                txid: *txid,
                location: TxLocation {
                    height: *height,
                    tx_index: *idx,
                },
            })
            .collect())
    }
}

impl MergeAppend for TxidLocationIndex {}

impl Schema<Vec<Vec<TxidLocationEntry>>> for TxidLocationIndex {
    fn into_entries(batches: Vec<Vec<TxidLocationEntry>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|e| (e.txid, e.location))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<TxidLocationEntry>> {
        vec![entries
            .into_iter()
            .map(|(txid, location)| TxidLocationEntry { txid, location })
            .collect()]
    }
}

impl EntryCodec for TxidLocationIndex {
    type Key = TransactionId;
    type Value = TxLocation;
    const VERSION: FormatVersion = FormatVersion(1);

    fn encode_key(key: &TransactionId) -> Vec<u8> {
        <[u8; 32]>::from(*key).to_vec()
    }

    fn encode_value(value: &TxLocation) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&value.height.value().to_le_bytes());
        buf.extend_from_slice(&value.tx_index.to_le_bytes());
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<TransactionId, DecodeError> {
        let mut arr = [0u8; 32];
        if bytes.len() != 32 {
            return Err(DecodeError::Invalid(format!(
                "expected 32, got {}",
                bytes.len()
            )));
        }
        arr.copy_from_slice(bytes);
        Ok(TransactionId::from(arr))
    }

    fn decode_value(bytes: &[u8]) -> Result<TxLocation, DecodeError> {
        if bytes.len() != 12 {
            return Err(DecodeError::Invalid(format!(
                "expected 12, got {}",
                bytes.len()
            )));
        }
        let height = BlockHeight::new(u64::from_le_bytes(bytes[0..8].try_into().expect("8")));
        let tx_index = u32::from_le_bytes(bytes[8..12].try_into().expect("4"));
        Ok(TxLocation { height, tx_index })
    }
}
