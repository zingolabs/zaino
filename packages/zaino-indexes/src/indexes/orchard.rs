//! OrchardIndex (BlockLocal × Append): height → compact orchard data per block.

use zaino_primitives::types::{EncryptedCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Compact orchard data for one transaction.
#[derive(Debug, Clone)]
pub struct OrchardTxCompact {
    /// Orchard actions: (nullifier, cmx, epk, enc_ciphertext_52bytes).
    pub actions: Vec<(Nullifier, NoteCommitment, EphemeralKey, EncryptedCiphertext)>,
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
#[derive(Debug, Clone)]
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
    fn extract(ctx: &OrchardCtx) -> Result<Self::Delta, ExtractError> {
        Ok(OrchardEntry {
            height: ctx.height,
            value: OrchardBlockValue(ctx.txs.clone()),
        })
    }
}

impl MergeAppend for OrchardIndex {}

impl Schema<Vec<OrchardEntry>> for OrchardIndex {
    type Key = BlockHeight;
    type Value = OrchardBlockValue;

    fn into_entries(entries: Vec<OrchardEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<OrchardEntry> {
        entries.into_iter().map(|(h, v)| OrchardEntry { height: h, value: v }).collect()
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> { key.value().to_le_bytes().to_vec() }

    fn encode_value(value: &OrchardBlockValue) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(value.0.len() as u32).to_le_bytes());
        for tx in &value.0 {
            buf.extend_from_slice(&(tx.actions.len() as u32).to_le_bytes());
            for (nf, cmx, epk, enc) in &tx.actions {
                buf.extend_from_slice(&<[u8; 32]>::from(*nf));
                buf.extend_from_slice(&<[u8; 32]>::from(*cmx));
                buf.extend_from_slice(&<[u8; 32]>::from(*epk));
                let enc_bytes: Vec<u8> = enc.clone().into();
                buf.extend_from_slice(&(enc_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&enc_bytes);
            }
        }
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| SchemaDecodeError::Invalid("bad height".into()))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(_bytes: &[u8]) -> Result<OrchardBlockValue, SchemaDecodeError> {
        Err(SchemaDecodeError::Invalid("orchard decode not yet implemented".into()))
    }
}
