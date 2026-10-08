//! TransparentSpendsIndex (BlockLocal × Append): outpoint → spending txid.
//!
//! For each transparent input in each transaction, records which
//! transaction spent that outpoint.

use zaino_persistence_codec::keys::HashKey;
use zaino_persistence_codec::{
    decode_value, encode_key, DecodeError, EntryCodec, KeyOrder, PersistentRecord,
};
use zaino_primitives::types::{OutputIndex, TransactionId};
use zaino_sync::backend::{BackendReader, ReadError};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::IndexId;
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context: the block's transparent inputs.
pub struct SpendCtx {
    /// All transparent spends in the block: (prev_txid, prev_index, spending_txid).
    pub spends: Vec<(TransactionId, OutputIndex, TransactionId)>,
}

/// One spend entry.
pub struct SpendEntry {
    /// The outpoint being spent (txid + output index).
    pub prev_txid: TransactionId,
    /// Output index within the previous transaction.
    pub prev_index: OutputIndex,
    /// The transaction that spent this outpoint.
    pub spending_txid: TransactionId,
}

/// Persisted key: prev_txid(32) + prev_index(4) = 36 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutpointKey {
    /// Previous transaction hash.
    pub prev_txid: TransactionId,
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
    type Error = std::convert::Infallible;

    fn extract(ctx: &SpendCtx) -> Result<Self::Delta, Self::Error> {
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
}

impl EntryCodec for TransparentSpendsIndex {
    type Key = OutpointKey;
    type Value = TransactionId;
    type PersistentKey = PersistentOutpointKey;
    // The value is a plain 32-byte spending txid — reuse the shared hash record.
    type PersistentValue = HashKey<TransactionId>;

    const KEY_ORDER: KeyOrder = KeyOrder::Scattered;

    fn fingerprint_samples() -> Vec<(OutpointKey, TransactionId)> {
        vec![(
            OutpointKey {
                prev_txid: TransactionId::from([1u8; 32]),
                prev_index: 2,
            },
            TransactionId::from([3u8; 32]),
        )]
    }
}

/// On-disk outpoint record: `prev_txid(32) ++ prev_index(4 LE)` = 36 bytes.
#[derive(PersistentRecord)]
pub struct PersistentOutpointKey {
    prev_txid: [u8; 32],
    prev_index: u32,
}

impl PersistentRecord for PersistentOutpointKey {
    type Domain = OutpointKey;

    fn from_domain(domain: &OutpointKey) -> Self {
        Self {
            prev_txid: <[u8; 32]>::from(domain.prev_txid),
            prev_index: domain.prev_index,
        }
    }

    fn into_domain(self) -> Result<OutpointKey, DecodeError> {
        Ok(OutpointKey {
            prev_txid: TransactionId::from(self.prev_txid),
            prev_index: self.prev_index,
        })
    }
}

/// A read of the transparent-spends index failed.
#[derive(Debug, thiserror::Error)]
pub enum SpenderReadError {
    /// The backend read failed.
    #[error(transparent)]
    Backend(#[from] ReadError),
    /// A persisted entry could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

/// The transaction that spent `outpoint`, or `None` if this index holds no
/// spend of it — the read side of this index.
///
/// A point get on the outpoint key, not a scan: the index is keyed by exactly
/// the question asked. `None` means "no spend recorded here", which is not the
/// same as "unspent" — an outpoint the indexed range never created is also
/// absent. Distinguishing the two needs the output's existence, which this
/// index does not carry.
pub fn read_spender(
    reader: &dyn BackendReader,
    outpoint: &OutpointKey,
) -> Result<Option<TransactionId>, SpenderReadError> {
    let key = encode_key::<TransparentSpendsIndex>(outpoint);
    match reader.get(ID.into(), &key)? {
        Some(bytes) => Ok(Some(decode_value::<TransparentSpendsIndex>(&bytes)?)),
        None => Ok(None),
    }
}
