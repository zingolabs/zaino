//! TransparentDataIndex (BlockLocal × Append): height → compact transparent data per block.

use zaino_primitives::types::{OutputIndex, Script, TransactionHash, Zatoshis};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Compact transparent data for one transaction.
#[derive(Debug, Clone)]
pub struct TransparentTxCompact {
    /// Transparent inputs: (prev_txid, prev_index).
    pub inputs: Vec<(TransactionHash, OutputIndex)>,
    /// Transparent outputs: (value, script).
    pub outputs: Vec<(Zatoshis, Script)>,
}

/// Per-index context.
pub struct TransparentDataCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Per-tx transparent data.
    pub txs: Vec<TransparentTxCompact>,
}

/// Delta.
pub struct TransparentDataEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Value.
    pub value: TransparentBlockValue,
}

/// Persisted value.
#[derive(Debug, Clone)]
pub struct TransparentBlockValue(pub Vec<TransparentTxCompact>);

/// Index definition.
pub struct TransparentDataIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("transparent_data");

impl IndexDef for TransparentDataIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = TransparentDataEntry;
    type BlockContext = TransparentDataCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for TransparentDataIndex {
    fn extract(ctx: &TransparentDataCtx) -> Result<Self::Delta, ExtractError> {
        Ok(TransparentDataEntry {
            height: ctx.height,
            value: TransparentBlockValue(ctx.txs.clone()),
        })
    }
}

impl MergeAppend for TransparentDataIndex {}

impl Schema<Vec<TransparentDataEntry>> for TransparentDataIndex {
    type Key = BlockHeight;
    type Value = TransparentBlockValue;

    fn into_entries(entries: Vec<TransparentDataEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TransparentDataEntry> {
        entries.into_iter().map(|(h, v)| TransparentDataEntry { height: h, value: v }).collect()
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> { key.value().to_le_bytes().to_vec() }

    fn encode_value(value: &TransparentBlockValue) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(value.0.len() as u32).to_le_bytes());
        for tx in &value.0 {
            buf.extend_from_slice(&(tx.inputs.len() as u32).to_le_bytes());
            for (txid, idx) in &tx.inputs {
                buf.extend_from_slice(&<[u8; 32]>::from(*txid));
                buf.extend_from_slice(&idx.to_le_bytes());
            }
            buf.extend_from_slice(&(tx.outputs.len() as u32).to_le_bytes());
            for (value, script) in &tx.outputs {
                buf.extend_from_slice(&u64::from(*value).to_le_bytes());
                let script_bytes: Vec<u8> = script.clone().into();
                buf.extend_from_slice(&(script_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&script_bytes);
            }
        }
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| SchemaDecodeError::Invalid("bad height".into()))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(_bytes: &[u8]) -> Result<TransparentBlockValue, SchemaDecodeError> {
        Err(SchemaDecodeError::Invalid("transparent_data decode not yet implemented".into()))
    }
}
