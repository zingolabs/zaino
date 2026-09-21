//! TransparentDataIndex (BlockLocal × Append): height → compact transparent data per block.

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord, RecordLayout};
use zaino_primitives::types::{OutputIndex, Script, TransactionId, Zatoshis};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

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
    type Error = std::convert::Infallible;

    fn extract(ctx: &TransparentDataCtx) -> Result<Self::Delta, Self::Error> {
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
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentTransparentValue;

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
}

/// On-disk transparent record. Layout: `tx_count(4 LE)`, then per tx
/// `input_count(4 LE)` then that many `prev_txid(32) ++ prev_index(4 LE)`, then
/// `output_count(4 LE)` then that many `value(8 LE) ++ script(len-prefixed)`.
pub struct PersistentTransparentValue {
    txs: Vec<PersistentTransparentTx>,
}

struct PersistentTransparentTx {
    inputs: Vec<PersistentTransparentInput>,
    outputs: Vec<PersistentTransparentOutput>,
}

struct PersistentTransparentInput {
    prev_txid: [u8; 32],
    prev_index: u32,
}

struct PersistentTransparentOutput {
    value: u64,
    script: Vec<u8>,
}

impl PersistentRecord for PersistentTransparentValue {
    type Domain = TransparentBlockValue;

    fn from_domain(domain: &TransparentBlockValue) -> Self {
        let txs = domain
            .0
            .iter()
            .map(|tx| PersistentTransparentTx {
                inputs: tx
                    .inputs
                    .iter()
                    .map(|(txid, idx)| PersistentTransparentInput {
                        prev_txid: <[u8; 32]>::from(*txid),
                        prev_index: *idx,
                    })
                    .collect(),
                outputs: tx
                    .outputs
                    .iter()
                    .map(|(value, script)| PersistentTransparentOutput {
                        value: u64::from(*value),
                        script: script.clone().into(),
                    })
                    .collect(),
            })
            .collect();
        Self { txs }
    }

    fn into_domain(self) -> Result<TransparentBlockValue, DecodeError> {
        let mut txs = Vec::with_capacity(self.txs.len());
        for tx in self.txs {
            let inputs = tx
                .inputs
                .into_iter()
                .map(|input| {
                    let prev_index: OutputIndex = input.prev_index;
                    (TransactionId::from(input.prev_txid), prev_index)
                })
                .collect();
            let mut outputs = Vec::with_capacity(tx.outputs.len());
            for output in tx.outputs {
                let value = Zatoshis::new(output.value)
                    .map_err(|e| DecodeError::Invalid(format!("output value: {e}")))?;
                outputs.push((value, Script::from(output.script)));
            }
            txs.push(TransparentTxCompact { inputs, outputs });
        }
        Ok(TransparentBlockValue(txs))
    }
}

// Nested count-prefixed collections (tx list, per-tx input and output lists) are
// irregular framing, so this record implements the byte layout by hand rather
// than deriving it.
impl RecordLayout for PersistentTransparentValue {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.count(self.txs.len());
        for tx in &self.txs {
            writer.count(tx.inputs.len());
            for input in &tx.inputs {
                writer.bytes32(&input.prev_txid);
                writer.u32(input.prev_index);
            }
            writer.count(tx.outputs.len());
            for output in &tx.outputs {
                writer.u64(output.value);
                writer.len_prefixed(&output.script);
            }
        }
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::with_capacity(tx_count);
        for _ in 0..tx_count {
            let input_count = cursor.count()?;
            let mut inputs = Vec::with_capacity(input_count);
            for _ in 0..input_count {
                inputs.push(PersistentTransparentInput {
                    prev_txid: cursor.bytes32()?,
                    prev_index: cursor.u32()?,
                });
            }
            let output_count = cursor.count()?;
            let mut outputs = Vec::with_capacity(output_count);
            for _ in 0..output_count {
                outputs.push(PersistentTransparentOutput {
                    value: cursor.u64()?,
                    script: cursor.len_prefixed()?,
                });
            }
            txs.push(PersistentTransparentTx { inputs, outputs });
        }
        cursor.finish()?;
        Ok(Self { txs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value};

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
        let bytes = encode_value::<TransparentDataIndex>(&value);
        assert_eq!(
            decode_value::<TransparentDataIndex>(&bytes).expect("decode"),
            value
        );
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = encode_value::<TransparentDataIndex>(&sample());
        assert!(decode_value::<TransparentDataIndex>(&bytes[..bytes.len() - 1]).is_err());
    }
}
