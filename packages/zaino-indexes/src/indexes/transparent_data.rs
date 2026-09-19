//! TransparentDataIndex (BlockLocal × Append): height → compact transparent data per block.

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{OutputIndex, Script, TransactionId, Zatoshis};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

use crate::indexes::decode::Cursor;

/// Compact transparent data for one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentTxCompact {
    /// Transparent inputs: (prev_txid, prev_index).
    pub inputs: Vec<(TransactionId, OutputIndex)>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn into_entries(entries: Vec<TransparentDataEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TransparentDataEntry> {
        entries
            .into_iter()
            .map(|(h, v)| TransparentDataEntry {
                height: h,
                value: v,
            })
            .collect()
    }
}

impl EntryCodec for TransparentDataIndex {
    type Key = BlockHeight;
    type Value = TransparentBlockValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, TransparentBlockValue)> {
        vec![(
            BlockHeight::new(1),
            TransparentBlockValue(vec![TransparentTxCompact {
                inputs: vec![(TransactionId::from([2u8; 32]), 3)],
                outputs: vec![(
                    Zatoshis::new(4).expect("valid"),
                    Script::from(vec![5u8, 6, 7]),
                )],
            }]),
        )]
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

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

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, DecodeError> {
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| DecodeError::Invalid("bad height".into()))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(bytes: &[u8]) -> Result<TransparentBlockValue, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::new();
        for _ in 0..tx_count {
            let input_count = cursor.count()?;
            let mut inputs = Vec::new();
            for _ in 0..input_count {
                let prev_txid = TransactionId::from(cursor.array::<32>()?);
                let prev_index: OutputIndex = cursor.u32()?;
                inputs.push((prev_txid, prev_index));
            }
            let output_count = cursor.count()?;
            let mut outputs = Vec::new();
            for _ in 0..output_count {
                let value = Zatoshis::new(cursor.u64()?)
                    .map_err(|e| DecodeError::Invalid(format!("output value: {e}")))?;
                let script = Script::from(cursor.len_prefixed()?);
                outputs.push((value, script));
            }
            txs.push(TransparentTxCompact { inputs, outputs });
        }
        cursor.finish()?;
        Ok(TransparentBlockValue(txs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TransparentBlockValue {
        TransparentBlockValue(vec![
            TransparentTxCompact {
                inputs: vec![
                    (TransactionId::from([1u8; 32]), 0),
                    (TransactionId::from([2u8; 32]), 7),
                ],
                outputs: vec![(
                    Zatoshis::new(100).expect("valid"),
                    Script::from(vec![1, 2, 3]),
                )],
            },
            TransparentTxCompact {
                inputs: vec![(TransactionId::from([9u8; 32]), 42)],
                outputs: vec![
                    (Zatoshis::new(0).expect("valid"), Script::from(vec![])),
                    (Zatoshis::new(555).expect("valid"), Script::from(vec![9, 8])),
                ],
            },
        ])
    }

    #[test]
    fn round_trips() {
        let value = sample();
        let bytes = TransparentDataIndex::encode_value(&value);
        assert_eq!(
            TransparentDataIndex::decode_value(&bytes).expect("decode"),
            value
        );
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = TransparentDataIndex::encode_value(&sample());
        assert!(TransparentDataIndex::decode_value(&bytes[..bytes.len() - 1]).is_err());
    }
}
