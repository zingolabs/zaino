//! OrchardIndex (BlockLocal × Append): height → compact orchard data per block.

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord};
use zaino_primitives::types::{CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

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
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentOrchardValue;

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
}

/// On-disk orchard record. Layout: `tx_count(4 LE)`, then per tx
/// `action_count(4 LE)`, then per action `nullifier(32) ++ cmx(32) ++ epk(32) ++
/// enc(len-prefixed)`.
pub struct PersistentOrchardValue {
    txs: Vec<PersistentOrchardTx>,
}

struct PersistentOrchardTx {
    actions: Vec<PersistentOrchardAction>,
}

struct PersistentOrchardAction {
    nullifier: [u8; 32],
    cmx: [u8; 32],
    epk: [u8; 32],
    enc: Vec<u8>,
}

impl PersistentRecord for PersistentOrchardValue {
    type Domain = OrchardBlockValue;

    fn from_domain(domain: &OrchardBlockValue) -> Self {
        let txs = domain
            .0
            .iter()
            .map(|tx| PersistentOrchardTx {
                actions: tx
                    .actions
                    .iter()
                    .map(|(nf, cmx, epk, enc)| PersistentOrchardAction {
                        nullifier: <[u8; 32]>::from(*nf),
                        cmx: <[u8; 32]>::from(*cmx),
                        epk: <[u8; 32]>::from(*epk),
                        enc: <[u8; CompactCiphertext::LENGTH]>::from(*enc).to_vec(),
                    })
                    .collect(),
            })
            .collect();
        Self { txs }
    }

    fn into_domain(self) -> Result<OrchardBlockValue, DecodeError> {
        let mut txs = Vec::with_capacity(self.txs.len());
        for tx in self.txs {
            let mut actions = Vec::with_capacity(tx.actions.len());
            for action in tx.actions {
                let enc_ciphertext = CompactCiphertext::try_new(&action.enc)
                    .map_err(|e| DecodeError::Invalid(format!("enc_ciphertext: {e}")))?;
                actions.push((
                    Nullifier::from(action.nullifier),
                    NoteCommitment::from(action.cmx),
                    EphemeralKey::from(action.epk),
                    enc_ciphertext,
                ));
            }
            txs.push(OrchardTxCompact { actions });
        }
        Ok(OrchardBlockValue(txs))
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.count(self.txs.len());
        for tx in &self.txs {
            writer.count(tx.actions.len());
            for action in &tx.actions {
                writer.bytes32(&action.nullifier);
                writer.bytes32(&action.cmx);
                writer.bytes32(&action.epk);
                writer.len_prefixed(&action.enc);
            }
        }
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let tx_count = cursor.count()?;
        let mut txs = Vec::with_capacity(tx_count);
        for _ in 0..tx_count {
            let action_count = cursor.count()?;
            let mut actions = Vec::with_capacity(action_count);
            for _ in 0..action_count {
                actions.push(PersistentOrchardAction {
                    nullifier: cursor.bytes32()?,
                    cmx: cursor.bytes32()?,
                    epk: cursor.bytes32()?,
                    enc: cursor.len_prefixed()?,
                });
            }
            txs.push(PersistentOrchardTx { actions });
        }
        cursor.finish()?;
        Ok(Self { txs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value};

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
        let bytes = encode_value::<OrchardIndex>(&value);
        assert_eq!(decode_value::<OrchardIndex>(&bytes).expect("decode"), value);
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = encode_value::<OrchardIndex>(&sample());
        assert!(decode_value::<OrchardIndex>(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn value_encodes_to_a_pinned_layout() {
        // Golden vector for one tx with a single action, then an empty tx.
        // tx_count=1 tx: [action_count=1, nf(32), cmx(32), epk(32), enc_len, enc],
        // then tx_count continues... here the whole value has 2 txs.
        let value = OrchardBlockValue(vec![
            OrchardTxCompact {
                actions: vec![(
                    Nullifier::from([0xA1; 32]),
                    NoteCommitment::from([0xB2; 32]),
                    EphemeralKey::from([0xC3; 32]),
                    CompactCiphertext::from([0xD4; CompactCiphertext::LENGTH]),
                )],
            },
            OrchardTxCompact { actions: vec![] },
        ]);
        let mut expected = Vec::new();
        expected.extend_from_slice(&2u32.to_le_bytes()); // tx_count
        expected.extend_from_slice(&1u32.to_le_bytes()); // tx0 action_count
        expected.extend_from_slice(&[0xA1; 32]);
        expected.extend_from_slice(&[0xB2; 32]);
        expected.extend_from_slice(&[0xC3; 32]);
        let enc_len = u32::try_from(CompactCiphertext::LENGTH).expect("fits");
        expected.extend_from_slice(&enc_len.to_le_bytes());
        expected.extend_from_slice(&[0xD4; CompactCiphertext::LENGTH]);
        expected.extend_from_slice(&0u32.to_le_bytes()); // tx1 action_count

        let bytes = encode_value::<OrchardIndex>(&value);
        assert_eq!(bytes, expected);
        assert_eq!(decode_value::<OrchardIndex>(&bytes).expect("decode"), value);
    }
}
