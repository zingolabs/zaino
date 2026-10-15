//! TxSetMembersIndex (BlockLocal × Append): txid → output indices that
//! qualify for the UTXO-set commitment (`txout_set`) at creation.
//!
//! Precomputed once, when a transaction's outputs are first seen, so
//! `txout_set`'s `empties_transaction` check can ask "which of this
//! transaction's outputs matter" with a single point lookup instead of
//! locating and decoding the whole creating block. Only transactions with at
//! least one qualifying output get an entry.
//!
//! An output's eligibility is the same P2PKH/P2SH test `txout_set` itself
//! applies ([`zaino_primitives::types::classify_script`]). It is duplicated
//! here rather than shared, because dependencies form a DAG and `txout_set`
//! depends on this index — the check itself is a cheap script-prefix read,
//! not the commitment-specific digest logic, so duplicating it costs nothing
//! and avoids an inverted dependency.

use zaino_persistence_codec::keys::HashKey;
use zaino_persistence_codec::{DecodeError, EntryCodec, KeyOrder, RecordLayout};
use zaino_primitives::types::{classify_script, OutputIndex, Script, ScriptType, TransactionId};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::IndexId;
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

use super::transparent_data::TransparentTxCompact;

/// Per-index context: the block's own transactions, each with its id and
/// transparent outputs.
pub struct TxSetMembersCtx {
    /// Transaction ids in block order.
    pub txids: Vec<TransactionId>,
    /// Per-tx transparent data, parallel to `txids`.
    pub txs: Vec<TransparentTxCompact>,
}

/// One transaction's qualifying output indices.
pub struct TxSetMembersEntry {
    /// The transaction.
    pub txid: TransactionId,
    /// Its output indices that qualify for the set, in ascending order.
    pub members: Vec<OutputIndex>,
}

/// Whether an output's script qualifies it for the UTXO-set commitment.
fn is_set_member(script: &Script) -> bool {
    let (_, script_type) = classify_script(script.as_bytes());
    !matches!(script_type, ScriptType::NonStandard)
}

/// Index definition.
pub struct TxSetMembersIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("tx_set_members");

impl IndexDef for TxSetMembersIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<TxSetMembersEntry>;
    type BlockContext = TxSetMembersCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for TxSetMembersIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &TxSetMembersCtx) -> Result<Self::Delta, Self::Error> {
        Ok(ctx
            .txids
            .iter()
            .zip(&ctx.txs)
            .filter_map(|(txid, tx)| {
                let members: Vec<OutputIndex> = tx
                    .outputs
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, script))| is_set_member(script))
                    // A position that does not fit `u32` cannot occur in a
                    // block that parsed (consensus block-size limits bound
                    // transaction output counts far below `u32::MAX`), and
                    // there is no set membership to record for one that
                    // could not be indexed.
                    .filter_map(|(position, _)| OutputIndex::try_from(position).ok())
                    .collect();
                if members.is_empty() {
                    None
                } else {
                    Some(TxSetMembersEntry {
                        txid: *txid,
                        members,
                    })
                }
            })
            .collect())
    }
}

impl MergeAppend for TxSetMembersIndex {}

impl Schema<Vec<Vec<TxSetMembersEntry>>> for TxSetMembersIndex {
    fn into_entries(batches: Vec<Vec<TxSetMembersEntry>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|e| (e.txid, TxSetMembersValue(e.members)))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<TxSetMembersEntry>> {
        vec![entries
            .into_iter()
            .map(|(txid, value)| TxSetMembersEntry {
                txid,
                members: value.0,
            })
            .collect()]
    }
}

/// Persisted value: a transaction's qualifying output indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSetMembersValue(pub Vec<OutputIndex>);

impl EntryCodec for TxSetMembersIndex {
    type Key = TransactionId;
    type Value = TxSetMembersValue;
    type PersistentKey = HashKey<TransactionId>;
    type PersistentValue = PersistentTxSetMembersValue;

    const KEY_ORDER: KeyOrder = KeyOrder::Scattered;

    fn fingerprint_samples() -> Vec<(TransactionId, TxSetMembersValue)> {
        vec![(
            TransactionId::from([1u8; 32]),
            TxSetMembersValue(vec![0, 2, 5]),
        )]
    }
}

/// On-disk record: concatenated 4-byte LE output indices, no count prefix —
/// the length is recovered by the multiple-of-4 division.
pub struct PersistentTxSetMembersValue(Vec<u32>);

impl zaino_persistence_codec::PersistentRecord for PersistentTxSetMembersValue {
    type Domain = TxSetMembersValue;

    fn from_domain(domain: &TxSetMembersValue) -> Self {
        Self(domain.0.clone())
    }

    fn into_domain(self) -> Result<TxSetMembersValue, DecodeError> {
        Ok(TxSetMembersValue(self.0))
    }
}

impl RecordLayout for PersistentTxSetMembersValue {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.len() * 4);
        for index in &self.0 {
            buf.extend_from_slice(&index.to_le_bytes());
        }
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if !bytes.len().is_multiple_of(4) {
            return Err(DecodeError::Invalid(format!(
                "tx-set-members length {} not multiple of 4",
                bytes.len()
            )));
        }
        let indices = bytes
            .chunks_exact(4)
            .map(|chunk| {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(chunk);
                u32::from_le_bytes(arr)
            })
            .collect();
        Ok(Self(indices))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value};

    #[test]
    fn round_trips() {
        let value = TxSetMembersValue(vec![0, 1, 41]);
        let bytes = encode_value::<TxSetMembersIndex>(&value);
        assert_eq!(
            decode_value::<TxSetMembersIndex>(&bytes).expect("decode"),
            value
        );
    }

    #[test]
    fn a_truncated_buffer_is_rejected() {
        let bytes = encode_value::<TxSetMembersIndex>(&TxSetMembersValue(vec![0, 1]));
        assert!(decode_value::<TxSetMembersIndex>(&bytes[..bytes.len() - 1]).is_err());
    }
}
