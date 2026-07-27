//! TxidsIndex (BlockLocal × Append): height → list of transaction ids.

use zaino_primitives::types::TransactionHash;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Per-index context.
pub struct TxidsCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Transaction ids in block order.
    pub txids: Vec<TransactionHash>,
}

/// Delta.
pub struct TxidsEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Txids (value).
    pub txids: Vec<TransactionHash>,
}

/// Persisted value: concatenated 32-byte txids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxidsValue(pub Vec<TransactionHash>);

/// Index definition.
pub struct TxidsIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("txids");

impl IndexDef for TxidsIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = TxidsEntry;
    type BlockContext = TxidsCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for TxidsIndex {
    fn extract(ctx: &TxidsCtx) -> Result<Self::Delta, ExtractError> {
        Ok(TxidsEntry {
            height: ctx.height,
            txids: ctx.txids.clone(),
        })
    }
}

impl MergeAppend for TxidsIndex {}

impl Schema<Vec<TxidsEntry>> for TxidsIndex {
    type Key = BlockHeight;
    type Value = TxidsValue;

    fn into_entries(entries: Vec<TxidsEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|e| (e.height, TxidsValue(e.txids)))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TxidsEntry> {
        entries
            .into_iter()
            .map(|(h, v)| TxidsEntry {
                height: h,
                txids: v.0,
            })
            .collect()
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

    fn encode_value(value: &TxidsValue) -> Vec<u8> {
        let mut buf = Vec::with_capacity(value.0.len() * 32);
        for txid in &value.0 {
            buf.extend_from_slice(&<[u8; 32]>::from(*txid));
        }
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| {
            SchemaDecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len()))
        })?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(bytes: &[u8]) -> Result<TxidsValue, SchemaDecodeError> {
        if !bytes.len().is_multiple_of(32) {
            return Err(SchemaDecodeError::Invalid(format!(
                "txids length {} not multiple of 32",
                bytes.len()
            )));
        }
        let txids = bytes
            .chunks_exact(32)
            .map(|c| {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(c);
                TransactionHash::from(arr)
            })
            .collect();
        Ok(TxidsValue(txids))
    }
}
