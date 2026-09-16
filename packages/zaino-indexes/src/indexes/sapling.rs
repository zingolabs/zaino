//! SaplingIndex (BlockLocal × Append): height → compact sapling data per block.

use zaino_primitives::types::{EncryptedCiphertext, EphemeralKey, NoteCommitment, Nullifier};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Compact sapling data for one transaction.
#[derive(Debug, Clone)]
pub struct SaplingTxCompact {
    /// Sapling spend nullifiers.
    pub nullifiers: Vec<Nullifier>,
    /// Sapling outputs: (cmu, epk, enc_ciphertext_52bytes).
    pub outputs: Vec<(NoteCommitment, EphemeralKey, EncryptedCiphertext)>,
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
#[derive(Debug, Clone)]
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
    type Key = BlockHeight;
    type Value = SaplingBlockValue;

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
                let enc_bytes: Vec<u8> = enc.clone().into();
                buf.extend_from_slice(&(enc_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&enc_bytes);
            }
        }
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| SchemaDecodeError::Invalid("bad height".into()))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(_bytes: &[u8]) -> Result<SaplingBlockValue, SchemaDecodeError> {
        // Full decode deferred — not needed for sync, only for serving.
        Err(SchemaDecodeError::Invalid(
            "sapling decode not yet implemented".into(),
        ))
    }
}
