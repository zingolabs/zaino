//! OutputValueIndex (BlockLocal × Append): outpoint → (value, script) at
//! creation.
//!
//! Every transparent output's value and script, keyed by the outpoint it
//! creates — every output, not just the ones later decided to be in a UTXO
//! set. `txout_set` reads this to resolve a spend's value and script with a
//! single point lookup, instead of locating and decoding the whole
//! historical block `transparent_data` stores. Unfiltered on purpose:
//! deciding which outputs are "in the set" is `txout_set`'s own concern, not
//! duplicated here.

use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, KeyOrder, PersistentRecord, RecordLayout};
use zaino_primitives::types::{OutputIndex, Script, TransactionId, Zatoshis};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

use super::transparent_data::TransparentTxCompact;
use super::transparent_spends::{OutpointKey, PersistentOutpointKey};

/// Per-index context: the block's own transactions, each with its id and
/// transparent outputs.
pub struct OutputValueCtx {
    /// Block height, for error reporting only (not part of the key).
    pub height: BlockHeight,
    /// Transaction ids in block order.
    pub txids: Vec<TransactionId>,
    /// Per-tx transparent data, parallel to `txids`.
    pub txs: Vec<TransparentTxCompact>,
}

/// One output: the outpoint it occupies and its value and script.
pub struct OutputValueEntry {
    /// The transaction that created this output.
    pub txid: TransactionId,
    /// Output index within that transaction.
    pub index: OutputIndex,
    /// The output's value.
    pub value: Zatoshis,
    /// The output's script.
    pub script: Script,
}

/// Persisted value: an output's value and script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputValue {
    /// The output's value.
    pub value: Zatoshis,
    /// The output's script.
    pub script: Script,
}

/// Why building a block's output-value entries failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OutputValueError {
    /// An output's position within its transaction did not fit `u32`.
    #[error("output position {position} at height {height} exceeds u32")]
    PositionOverflow {
        /// Block height.
        height: BlockHeight,
        /// The offending position.
        position: usize,
    },
}

/// Index definition.
pub struct OutputValueIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("output_value");

impl IndexDef for OutputValueIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<OutputValueEntry>;
    type BlockContext = OutputValueCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for OutputValueIndex {
    type Error = OutputValueError;

    fn extract(ctx: &OutputValueCtx) -> Result<Self::Delta, Self::Error> {
        let mut entries = Vec::new();
        for (txid, tx) in ctx.txids.iter().zip(&ctx.txs) {
            for (position, (value, script)) in tx.outputs.iter().enumerate() {
                let index = OutputIndex::try_from(position).map_err(|_| {
                    OutputValueError::PositionOverflow {
                        height: ctx.height,
                        position,
                    }
                })?;
                entries.push(OutputValueEntry {
                    txid: *txid,
                    index,
                    value: *value,
                    script: script.clone(),
                });
            }
        }
        Ok(entries)
    }
}

impl MergeAppend for OutputValueIndex {}

impl Schema<Vec<Vec<OutputValueEntry>>> for OutputValueIndex {
    fn into_entries(batches: Vec<Vec<OutputValueEntry>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|e| {
                (
                    OutpointKey {
                        prev_txid: e.txid,
                        prev_index: e.index,
                    },
                    OutputValue {
                        value: e.value,
                        script: e.script,
                    },
                )
            })
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<OutputValueEntry>> {
        vec![entries
            .into_iter()
            .map(|(key, value)| OutputValueEntry {
                txid: key.prev_txid,
                index: key.prev_index,
                value: value.value,
                script: value.script,
            })
            .collect()]
    }
}

impl EntryCodec for OutputValueIndex {
    type Key = OutpointKey;
    type Value = OutputValue;
    type PersistentKey = PersistentOutpointKey;
    type PersistentValue = PersistentOutputValue;

    const KEY_ORDER: KeyOrder = KeyOrder::Scattered;

    fn fingerprint_samples() -> Vec<(OutpointKey, OutputValue)> {
        vec![(
            OutpointKey {
                prev_txid: TransactionId::from([1u8; 32]),
                prev_index: 2,
            },
            OutputValue {
                value: Zatoshis::new(3).expect("valid"),
                script: Script::from(vec![4u8, 5, 6]),
            },
        )]
    }
}

/// On-disk output-value record: `value(8 LE) ++ script(len-prefixed)`.
pub struct PersistentOutputValue {
    value: u64,
    script: Vec<u8>,
}

impl PersistentRecord for PersistentOutputValue {
    type Domain = OutputValue;

    fn from_domain(domain: &OutputValue) -> Self {
        Self {
            value: u64::from(domain.value),
            script: domain.script.clone().into(),
        }
    }

    fn into_domain(self) -> Result<OutputValue, DecodeError> {
        let value = Zatoshis::new(self.value)
            .map_err(|e| DecodeError::Invalid(format!("output value: {e}")))?;
        Ok(OutputValue {
            value,
            script: Script::from(self.script),
        })
    }
}

// A length-prefixed script after a fixed-width value is irregular framing, so
// this record implements the byte layout by hand rather than deriving it.
impl RecordLayout for PersistentOutputValue {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.value);
        writer.len_prefixed(&self.script);
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let value = cursor.u64()?;
        let script = cursor.len_prefixed()?;
        cursor.finish()?;
        Ok(Self { value, script })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value};

    fn sample() -> OutputValue {
        OutputValue {
            value: Zatoshis::new(100).expect("valid"),
            script: Script::from(vec![1, 2, 3]),
        }
    }

    #[test]
    fn round_trips() {
        let value = sample();
        let bytes = encode_value::<OutputValueIndex>(&value);
        assert_eq!(
            decode_value::<OutputValueIndex>(&bytes).expect("decode"),
            value
        );
    }

    #[test]
    fn round_trips_an_empty_script() {
        let value = OutputValue {
            value: Zatoshis::new(0).expect("valid"),
            script: Script::from(vec![]),
        };
        let bytes = encode_value::<OutputValueIndex>(&value);
        assert_eq!(
            decode_value::<OutputValueIndex>(&bytes).expect("decode"),
            value
        );
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = encode_value::<OutputValueIndex>(&sample());
        assert!(decode_value::<OutputValueIndex>(&bytes[..bytes.len() - 1]).is_err());
    }
}
