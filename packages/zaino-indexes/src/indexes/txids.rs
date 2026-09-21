//! TxidsIndex (BlockLocal × Append): height → list of transaction ids.

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{DecodeError, EntryCodec, PersistentRecord};
use zaino_primitives::types::TransactionId;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context.
pub struct TxidsCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Transaction ids in block order.
    pub txids: Vec<TransactionId>,
}

/// Delta.
pub struct TxidsEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Txids (value).
    pub txids: Vec<TransactionId>,
}

/// Persisted value: concatenated 32-byte txids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxidsValue(pub Vec<TransactionId>);

/// Index definition.
pub struct TxidsIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("txids");

impl IndexDef for TxidsIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = TxidsEntry;
    type BlockContext = TxidsCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for TxidsIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &TxidsCtx) -> Result<Self::Delta, Self::Error> {
        Ok(TxidsEntry {
            height: ctx.height,
            txids: ctx.txids.clone(),
        })
    }
}

impl MergeAppend for TxidsIndex {}

impl Schema<Vec<TxidsEntry>> for TxidsIndex {
    fn into_entries(entries: Vec<TxidsEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|e| (e.height, TxidsValue(e.txids)))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TxidsEntry> {
        entries
            .into_iter()
            .map(|(h, v)| TxidsEntry {
                height: h,
                txids: v.0,
            })
            .collect()
    }
}

impl EntryCodec for TxidsIndex {
    type Key = BlockHeight;
    type Value = TxidsValue;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentTxidsValue;

    fn fingerprint_samples() -> Vec<(BlockHeight, TxidsValue)> {
        vec![(
            BlockHeight::new(1),
            TxidsValue(vec![
                TransactionId::from([1u8; 32]),
                TransactionId::from([2u8; 32]),
            ]),
        )]
    }
}

/// On-disk txids record: the 32-byte txids concatenated, no count prefix — the
/// length is recovered by the multiple-of-32 division.
pub struct PersistentTxidsValue(Vec<[u8; 32]>);

impl PersistentRecord for PersistentTxidsValue {
    type Domain = TxidsValue;

    fn from_domain(domain: &TxidsValue) -> Self {
        Self(
            domain
                .0
                .iter()
                .map(|txid| <[u8; 32]>::from(*txid))
                .collect(),
        )
    }

    fn into_domain(self) -> Result<TxidsValue, DecodeError> {
        Ok(TxidsValue(
            self.0.into_iter().map(TransactionId::from).collect(),
        ))
    }

    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.len() * 32);
        for txid in &self.0 {
            buf.extend_from_slice(txid);
        }
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if !bytes.len().is_multiple_of(32) {
            return Err(DecodeError::Invalid(format!(
                "txids length {} not multiple of 32",
                bytes.len()
            )));
        }
        let txids = bytes
            .chunks_exact(32)
            .map(|chunk| {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(chunk);
                arr
            })
            .collect();
        Ok(Self(txids))
    }
}
