//! SaplingIndex (BlockLocal × Append): height → compact sapling data per block.

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

use crate::indexes::decode::Cursor;

/// Compact sapling data for one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingTxCompact {
    /// Sapling spend nullifiers.
    pub nullifiers: Vec<Nullifier>,
    /// Sapling outputs: (cmu, epk, enc_ciphertext_52bytes).
    pub outputs: Vec<(NoteCommitment, EphemeralKey, CompactCiphertext)>,
}

/// Per-index context.
pub struct SaplingCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Per-tx sapling data.
    pub txs: Vec<SaplingTxCompact>,
}

/// Delta.
pub struct SaplingEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Value.
    pub value: SaplingBlockValue,
}

/// Persisted value: all sapling data for the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingBlockValue(pub Vec<SaplingTxCompact>);

/// Index definition.
pub struct SaplingIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("sapling");

impl IndexDef for SaplingIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = SaplingEntry;
    type BlockContext = SaplingCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for SaplingIndex {
    fn extract(ctx: &SaplingCtx) -> Result<Self::Delta, ExtractError> {
        Ok(SaplingEntry {
            height: ctx.height,
            value: SaplingBlockValue(ctx.txs.clone()),
        })
    }
}

impl MergeAppend for SaplingIndex {}

impl Schema<Vec<SaplingEntry>> for SaplingIndex {
    fn into_entries(entries: Vec<SaplingEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<SaplingEntry> {
        entries
            .into_iter()
            .map(|(h, v)| SaplingEntry {
                height: h,
                value: v,
            })
            .collect()
    }
}

impl EntryCodec for SaplingIndex {
    type Key = BlockHeight;
    type Value = SaplingBlockValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, SaplingBlockValue)> {
        vec![(
            BlockHeight::new(1),
            SaplingBlockValue(vec![SaplingTxCompact {
                nullifiers: vec![Nullifier::from([2u8; 32])],
                outputs: vec![(
                    NoteCommitment::from([3u8; 32]),
                    EphemeralKey::from([4u8; 32]),
                    CompactCiphertext::from([5u8; CompactCiphertext::LENGTH]),
                )],
            }]),
        )]
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

    fn encode_value(value: &SaplingBlockValue) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(value.0.len() as u32).to_le_bytes());
        for tx in &value.0 {
            buf.extend_from_slice(&(tx.nullifiers.len() as u32).to_le_bytes());
            for nf in &tx.nullifiers {
                buf.extend_from_slice(&<[u8; 32]>::from(*nf));
            }
            buf.extend_from_slice(&(tx.outputs.len() as u32).to_le_bytes());
            for (cmu, epk, enc) in &tx.outputs {
                buf.extend_from_slice(&<[u8; 32]>::from(*cmu));
                buf.extend_from_slice(&<[u8; 32]>::from(*epk));
                let enc_bytes = <[u8; CompactCiphertext::LENGTH]>::from(*enc).to_vec();
                buf.extend_from_slice(&(enc_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&enc_bytes);
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

    fn decode_value(bytes: &[u8]) -> Result<SaplingBlockValue, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::new();
        for _ in 0..tx_count {
            let nullifier_count = cursor.count()?;
            let mut nullifiers = Vec::new();
            for _ in 0..nullifier_count {
                nullifiers.push(Nullifier::from(cursor.array::<32>()?));
            }
            let output_count = cursor.count()?;
            let mut outputs = Vec::new();
            for _ in 0..output_count {
                let cmu = NoteCommitment::from(cursor.array::<32>()?);
                let ephemeral_key = EphemeralKey::from(cursor.array::<32>()?);
                let enc_ciphertext = CompactCiphertext::try_new(&cursor.len_prefixed()?)
                    .map_err(|e| DecodeError::Invalid(format!("enc_ciphertext: {e}")))?;
                outputs.push((cmu, ephemeral_key, enc_ciphertext));
            }
            txs.push(SaplingTxCompact {
                nullifiers,
                outputs,
            });
        }
        cursor.finish()?;
        Ok(SaplingBlockValue(txs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SaplingBlockValue {
        SaplingBlockValue(vec![
            SaplingTxCompact {
                nullifiers: vec![Nullifier::from([1u8; 32]), Nullifier::from([2u8; 32])],
                outputs: vec![(
                    NoteCommitment::from([3u8; 32]),
                    EphemeralKey::from([4u8; 32]),
                    CompactCiphertext::from([5u8; CompactCiphertext::LENGTH]),
                )],
            },
            SaplingTxCompact {
                nullifiers: vec![],
                outputs: vec![(
                    NoteCommitment::from([6u8; 32]),
                    EphemeralKey::from([7u8; 32]),
                    CompactCiphertext::from([8u8; CompactCiphertext::LENGTH]),
                )],
            },
        ])
    }

    #[test]
    fn round_trips() {
        let value = sample();
        let bytes = SaplingIndex::encode_value(&value);
        assert_eq!(SaplingIndex::decode_value(&bytes).expect("decode"), value);
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = SaplingIndex::encode_value(&sample());
        assert!(SaplingIndex::decode_value(&bytes[..bytes.len() - 1]).is_err());
    }
}
