//! SaplingIndex (BlockLocal × Append): height → compact sapling data per block.

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord, RecordLayout};
use zaino_primitives::types::{CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

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
    type Error = std::convert::Infallible;

    fn extract(ctx: &SaplingCtx) -> Result<Self::Delta, Self::Error> {
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
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentSaplingValue;

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
}

/// On-disk sapling record. Layout: `tx_count(4 LE)`, then per tx
/// `nullifier_count(4 LE)` then that many `nullifier(32)`, then
/// `output_count(4 LE)` then that many `cmu(32) ++ epk(32) ++ enc(len-prefixed)`.
pub struct PersistentSaplingValue {
    txs: Vec<PersistentSaplingTx>,
}

struct PersistentSaplingTx {
    nullifiers: Vec<[u8; 32]>,
    outputs: Vec<PersistentSaplingOutput>,
}

struct PersistentSaplingOutput {
    cmu: [u8; 32],
    epk: [u8; 32],
    enc: Vec<u8>,
}

impl PersistentRecord for PersistentSaplingValue {
    type Domain = SaplingBlockValue;

    fn from_domain(domain: &SaplingBlockValue) -> Self {
        let txs = domain
            .0
            .iter()
            .map(|tx| PersistentSaplingTx {
                nullifiers: tx
                    .nullifiers
                    .iter()
                    .map(|nf| <[u8; 32]>::from(*nf))
                    .collect(),
                outputs: tx
                    .outputs
                    .iter()
                    .map(|(cmu, epk, enc)| PersistentSaplingOutput {
                        cmu: <[u8; 32]>::from(*cmu),
                        epk: <[u8; 32]>::from(*epk),
                        enc: <[u8; CompactCiphertext::LENGTH]>::from(*enc).to_vec(),
                    })
                    .collect(),
            })
            .collect();
        Self { txs }
    }

    fn into_domain(self) -> Result<SaplingBlockValue, DecodeError> {
        let mut txs = Vec::with_capacity(self.txs.len());
        for tx in self.txs {
            let nullifiers = tx.nullifiers.into_iter().map(Nullifier::from).collect();
            let mut outputs = Vec::with_capacity(tx.outputs.len());
            for output in tx.outputs {
                let enc_ciphertext = CompactCiphertext::try_new(&output.enc)
                    .map_err(|e| DecodeError::Invalid(format!("enc_ciphertext: {e}")))?;
                outputs.push((
                    NoteCommitment::from(output.cmu),
                    EphemeralKey::from(output.epk),
                    enc_ciphertext,
                ));
            }
            txs.push(SaplingTxCompact {
                nullifiers,
                outputs,
            });
        }
        Ok(SaplingBlockValue(txs))
    }
}

// Nested count-prefixed collections (tx list, per-tx nullifier and output lists)
// are irregular framing, so this record implements the byte layout by hand
// rather than deriving it.
impl RecordLayout for PersistentSaplingValue {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.count(self.txs.len());
        for tx in &self.txs {
            writer.count(tx.nullifiers.len());
            for nullifier in &tx.nullifiers {
                writer.bytes32(nullifier);
            }
            writer.count(tx.outputs.len());
            for output in &tx.outputs {
                writer.bytes32(&output.cmu);
                writer.bytes32(&output.epk);
                writer.len_prefixed(&output.enc);
            }
        }
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::with_capacity(tx_count);
        for _ in 0..tx_count {
            let nullifier_count = cursor.count()?;
            let mut nullifiers = Vec::with_capacity(nullifier_count);
            for _ in 0..nullifier_count {
                nullifiers.push(cursor.bytes32()?);
            }
            let output_count = cursor.count()?;
            let mut outputs = Vec::with_capacity(output_count);
            for _ in 0..output_count {
                outputs.push(PersistentSaplingOutput {
                    cmu: cursor.bytes32()?,
                    epk: cursor.bytes32()?,
                    enc: cursor.len_prefixed()?,
                });
            }
            txs.push(PersistentSaplingTx {
                nullifiers,
                outputs,
            });
        }
        cursor.finish()?;
        Ok(Self { txs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value};

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
        let bytes = encode_value::<SaplingIndex>(&value);
        assert_eq!(decode_value::<SaplingIndex>(&bytes).expect("decode"), value);
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = encode_value::<SaplingIndex>(&sample());
        assert!(decode_value::<SaplingIndex>(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn value_encodes_to_a_pinned_layout() {
        // Golden vector: one tx, one nullifier, one output.
        let value = SaplingBlockValue(vec![SaplingTxCompact {
            nullifiers: vec![Nullifier::from([0x11; 32])],
            outputs: vec![(
                NoteCommitment::from([0x22; 32]),
                EphemeralKey::from([0x33; 32]),
                CompactCiphertext::from([0x44; CompactCiphertext::LENGTH]),
            )],
        }]);
        let mut expected = Vec::new();
        expected.extend_from_slice(&1u32.to_le_bytes()); // tx_count
        expected.extend_from_slice(&1u32.to_le_bytes()); // nullifier_count
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&1u32.to_le_bytes()); // output_count
        expected.extend_from_slice(&[0x22; 32]);
        expected.extend_from_slice(&[0x33; 32]);
        let enc_len = u32::try_from(CompactCiphertext::LENGTH).expect("fits");
        expected.extend_from_slice(&enc_len.to_le_bytes());
        expected.extend_from_slice(&[0x44; CompactCiphertext::LENGTH]);

        let bytes = encode_value::<SaplingIndex>(&value);
        assert_eq!(bytes, expected);
        assert_eq!(decode_value::<SaplingIndex>(&bytes).expect("decode"), value);
    }
}
