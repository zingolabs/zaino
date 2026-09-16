//! TransparentSpendsIndex (BlockLocal × Append): outpoint → spending txid.
//!
//! For each transparent input in each transaction, records which
//! transaction spent that outpoint.

use zaino_primitives::types::{OutputIndex, TransactionHash};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::IndexId;
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

/// Per-index context: the block's transparent inputs.
pub struct SpendCtx {
    /// All transparent spends in the block: (prev_txid, prev_index, spending_txid).
    pub spends: Vec<(TransactionHash, OutputIndex, TransactionHash)>,
}

/// One spend entry.
pub struct SpendEntry {
    /// The outpoint being spent (txid + output index).
    pub prev_txid: TransactionHash,
    /// Output index within the previous transaction.
    pub prev_index: OutputIndex,
    /// The transaction that spent this outpoint.
    pub spending_txid: TransactionHash,
}

/// Persisted key: prev_txid(32) + prev_index(4) = 36 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutpointKey {
    /// Previous transaction hash.
    pub prev_txid: TransactionHash,
    /// Output index.
    pub prev_index: OutputIndex,
}

/// Index definition.
pub struct TransparentSpendsIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("transparent_spends");

impl IndexDef for TransparentSpendsIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<SpendEntry>;
    type BlockContext = SpendCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for TransparentSpendsIndex {
    fn extract(ctx: &SpendCtx) -> Result<Self::Delta, ExtractError> {
        Ok(ctx
            .spends
            .iter()
            .map(|(prev_txid, prev_index, spending_txid)| SpendEntry {
                prev_txid: *prev_txid,
                prev_index: *prev_index,
                spending_txid: *spending_txid,
            })
            .collect())
    }
}

impl MergeAppend for TransparentSpendsIndex {}

impl Schema<Vec<Vec<SpendEntry>>> for TransparentSpendsIndex {
    type Key = OutpointKey;
    type Value = TransactionHash;

    fn into_entries(batches: Vec<Vec<SpendEntry>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|e| {
                (
                    OutpointKey {
                        prev_txid: e.prev_txid,
                        prev_index: e.prev_index,
                    },
                    e.spending_txid,
                )
            })
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<SpendEntry>> {
        vec![entries
            .into_iter()
            .map(|(key, spending_txid)| SpendEntry {
                prev_txid: key.prev_txid,
                prev_index: key.prev_index,
                spending_txid,
            })
            .collect()]
    }

    fn encode_key(key: &OutpointKey) -> Vec<u8> {
        let mut buf = Vec::with_capacity(36);
        buf.extend_from_slice(&<[u8; 32]>::from(key.prev_txid));
        buf.extend_from_slice(&key.prev_index.to_le_bytes());
        buf
    }

    fn encode_value(value: &TransactionHash) -> Vec<u8> {
        <[u8; 32]>::from(*value).to_vec()
    }

    fn decode_key(bytes: &[u8]) -> Result<OutpointKey, SchemaDecodeError> {
        if bytes.len() != 36 {
            return Err(SchemaDecodeError::Invalid(format!(
                "expected 36 bytes, got {}",
                bytes.len()
            )));
        }
        let mut txid = [0u8; 32];
        txid.copy_from_slice(&bytes[0..32]);
        let index = u32::from_le_bytes(bytes[32..36].try_into().expect("4 bytes"));
        Ok(OutpointKey {
            prev_txid: TransactionHash::from(txid),
            prev_index: index,
        })
    }

    fn decode_value(bytes: &[u8]) -> Result<TransactionHash, SchemaDecodeError> {
        if bytes.len() != 32 {
            return Err(SchemaDecodeError::Invalid(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut txid = [0u8; 32];
        txid.copy_from_slice(bytes);
        Ok(TransactionHash::from(txid))
    }
}
