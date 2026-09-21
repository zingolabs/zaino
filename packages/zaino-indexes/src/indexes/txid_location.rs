//! TxidLocationIndex (BlockLocal × Append): txid → (height, tx_index).

use zaino_persistence_codec::keys::HashKey;
use zaino_persistence_codec::layout::{Cursor, Writer};
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord};
use zaino_primitives::types::TransactionId;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context.
pub struct TxidLocationCtx {
    /// (txid, height, tx_index) for each transaction.
    pub locations: Vec<(TransactionId, BlockHeight, u32)>,
}

/// Delta: one entry per transaction.
pub struct TxidLocationEntry {
    /// Transaction hash (key).
    pub txid: TransactionId,
    /// Location (value).
    pub location: TxLocation,
}

/// Persisted value: height + tx_index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxLocation {
    /// Block height.
    pub height: BlockHeight,
    /// Transaction index within the block.
    pub tx_index: u32,
}

/// Index definition.
pub struct TxidLocationIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("txid_location");

impl IndexDef for TxidLocationIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<TxidLocationEntry>;
    type BlockContext = TxidLocationCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for TxidLocationIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &TxidLocationCtx) -> Result<Self::Delta, Self::Error> {
        Ok(ctx
            .locations
            .iter()
            .map(|(txid, height, idx)| TxidLocationEntry {
                txid: *txid,
                location: TxLocation {
                    height: *height,
                    tx_index: *idx,
                },
            })
            .collect())
    }
}

impl MergeAppend for TxidLocationIndex {}

impl Schema<Vec<Vec<TxidLocationEntry>>> for TxidLocationIndex {
    fn into_entries(batches: Vec<Vec<TxidLocationEntry>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|e| (e.txid, e.location))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<TxidLocationEntry>> {
        vec![entries
            .into_iter()
            .map(|(txid, location)| TxidLocationEntry { txid, location })
            .collect()]
    }
}

impl EntryCodec for TxidLocationIndex {
    type Key = TransactionId;
    type Value = TxLocation;
    type PersistentKey = HashKey<TransactionId>;
    type PersistentValue = PersistentTxLocation;

    fn fingerprint_samples() -> Vec<(TransactionId, TxLocation)> {
        vec![(
            TransactionId::from([1u8; 32]),
            TxLocation {
                height: BlockHeight::new(2),
                tx_index: 3,
            },
        )]
    }
}

/// On-disk transaction-location record: `height(8 LE) ++ tx_index(4 LE)` = 12
/// bytes.
pub struct PersistentTxLocation {
    height: u64,
    tx_index: u32,
}

impl PersistentRecord for PersistentTxLocation {
    type Domain = TxLocation;

    fn from_domain(domain: &TxLocation) -> Self {
        Self {
            height: domain.height.value(),
            tx_index: domain.tx_index,
        }
    }

    fn into_domain(self) -> Result<TxLocation, DecodeError> {
        Ok(TxLocation {
            height: BlockHeight::new(self.height),
            tx_index: self.tx_index,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(12);
        writer.u64(self.height);
        writer.u32(self.tx_index);
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        let height = cursor.u64()?;
        let tx_index = cursor.u32()?;
        cursor.finish()?;
        Ok(Self { height, tx_index })
    }
}
