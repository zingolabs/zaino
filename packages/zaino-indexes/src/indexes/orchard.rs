//! OrchardIndex (BlockLocal × Append): height → compact orchard data per block.

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

use crate::indexes::decode::Cursor;

/// Compact orchard data for one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardTxCompact {
    /// Orchard actions: (nullifier, cmx, epk, enc_ciphertext_52bytes).
    pub actions: Vec<(Nullifier, NoteCommitment, EphemeralKey, CompactCiphertext)>,
}

/// Per-index context.
pub struct OrchardCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Per-tx orchard data.
    pub txs: Vec<OrchardTxCompact>,
}

/// Delta.
pub struct OrchardEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Value.
    pub value: OrchardBlockValue,
}

/// Persisted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardBlockValue(pub Vec<OrchardTxCompact>);

/// Index definition.
pub struct OrchardIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("orchard");

impl IndexDef for OrchardIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = OrchardEntry;
    type BlockContext = OrchardCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for OrchardIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &OrchardCtx) -> Result<Self::Delta, Self::Error> {
        Ok(OrchardEntry {
            height: ctx.height,
            value: OrchardBlockValue(ctx.txs.clone()),
        })
    }
}

impl MergeAppend for OrchardIndex {}

impl Schema<Vec<OrchardEntry>> for OrchardIndex {
    fn into_entries(entries: Vec<OrchardEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<OrchardEntry> {
        entries
            .into_iter()
            .map(|(h, v)| OrchardEntry {
                height: h,
                value: v,
            })
            .collect()
    }
}

impl EntryCodec for OrchardIndex {
    type Key = BlockHeight;
    type Value = OrchardBlockValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, OrchardBlockValue)> {
        vec![(
            BlockHeight::new(1),
            OrchardBlockValue(vec![OrchardTxCompact {
                actions: vec![(
                    Nullifier::from([2u8; 32]),
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

    fn encode_value(value: &OrchardBlockValue) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(value.0.len() as u32).to_le_bytes());
        for tx in &value.0 {
            buf.extend_from_slice(&(tx.actions.len() as u32).to_le_bytes());
            for (nf, cmx, epk, enc) in &tx.actions {
                buf.extend_from_slice(&<[u8; 32]>::from(*nf));
                buf.extend_from_slice(&<[u8; 32]>::from(*cmx));
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

    fn decode_value(bytes: &[u8]) -> Result<OrchardBlockValue, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::new();
        for _ in 0..tx_count {
            let action_count = cursor.count()?;
            let mut actions = Vec::new();
            for _ in 0..action_count {
                let nullifier = Nullifier::from(cursor.array::<32>()?);
                let cmx = NoteCommitment::from(cursor.array::<32>()?);
                let ephemeral_key = EphemeralKey::from(cursor.array::<32>()?);
                let enc_ciphertext = CompactCiphertext::try_new(&cursor.len_prefixed()?)
                    .map_err(|e| DecodeError::Invalid(format!("enc_ciphertext: {e}")))?;
                actions.push((nullifier, cmx, ephemeral_key, enc_ciphertext));
            }
            txs.push(OrchardTxCompact { actions });
        }
        cursor.finish()?;
        Ok(OrchardBlockValue(txs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OrchardBlockValue {
        OrchardBlockValue(vec![
            OrchardTxCompact {
                actions: vec![
                    (
                        Nullifier::from([1u8; 32]),
                        NoteCommitment::from([2u8; 32]),
                        EphemeralKey::from([3u8; 32]),
                        CompactCiphertext::from([4u8; CompactCiphertext::LENGTH]),
                    ),
                    (
                        Nullifier::from([5u8; 32]),
                        NoteCommitment::from([6u8; 32]),
                        EphemeralKey::from([7u8; 32]),
                        CompactCiphertext::from([8u8; CompactCiphertext::LENGTH]),
                    ),
                ],
            },
            OrchardTxCompact { actions: vec![] },
        ])
    }

    #[test]
    fn round_trips() {
        let value = sample();
        let bytes = OrchardIndex::encode_value(&value);
        assert_eq!(OrchardIndex::decode_value(&bytes).expect("decode"), value);
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = OrchardIndex::encode_value(&sample());
        assert!(OrchardIndex::decode_value(&bytes[..bytes.len() - 1]).is_err());
    }
}
