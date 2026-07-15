//! The mempool entry — Zaino's protocol-correct view of one unconfirmed
//! transaction — and its wire conversions.

use std::sync::Arc;

use zebra_chain::{
    block::Height,
    transaction::{Hash as TxHash, SerializedTransaction},
};

/// One unconfirmed transaction in Zaino's mempool read model.
///
/// Mirrors the fields of Zebra's `VerifiedUnminedTx` that Zaino serves or needs:
/// the transaction bytes, its size, and — crucially — `entry_height`, the chain
/// tip height when the transaction entered the mempool (Zebra's
/// `VerifiedUnminedTx.height`, zcashd's `nHeight`). This is the protocol-correct
/// internal height; wire conversions translate it to each RPC's shape.
#[derive(Debug)]
pub struct MempoolEntry {
    /// The transaction's id.
    pub txid: TxHash,

    /// Serialized transaction bytes, guaranteed to deserialize as a Zcash
    /// transaction (they came from the validator).
    pub serialized_tx: Arc<SerializedTransaction>,

    /// Length of `serialized_tx` in bytes.
    pub raw_len: u32,

    /// Chain tip height when the transaction entered the mempool.
    ///
    /// Sourced from the validator (`getrawmempool verbose`) so it matches the
    /// validator's own `nHeight`. This is *not* the wire height for lightclient
    /// responses (which is the `0` "in mempool" sentinel); it is protocol
    /// metadata.
    pub entry_height: Height,

    /// Unix time (seconds) the transaction entered the mempool, when the source
    /// reports one.
    pub entry_time: Option<i64>,

    /// The mempool generation in which Zaino first observed this entry.
    pub first_seen_generation: u64,
}

impl MempoolEntry {
    /// The ZIP-401 cost of this entry, in bytes.
    pub fn cost(&self) -> u64 {
        crate::config::tx_cost(self.raw_len)
    }

    /// The raw serialized transaction bytes.
    pub fn serialized_bytes(&self) -> &[u8] {
        self.serialized_tx.as_ref().as_ref()
    }

    /// Convert to the lightclient [`RawTransaction`](zaino_proto::proto::service::RawTransaction)
    /// wire shape.
    ///
    /// Unconfirmed transactions carry wire `height = 0` — the proto-documented
    /// "in the mempool" sentinel, matching lightwalletd and Zaino's compact
    /// mempool responses. `entry_height` is deliberately *not* used here: it is
    /// internal protocol metadata, whereas the wire height field means "mined
    /// height, or 0 if in the mempool".
    pub fn to_lightclient_raw_transaction(&self) -> zaino_proto::proto::service::RawTransaction {
        zaino_proto::proto::service::RawTransaction {
            data: self.serialized_bytes().to_vec(),
            height: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(bytes: Vec<u8>, entry_height: u32) -> MempoolEntry {
        let raw_len = bytes.len() as u32;
        MempoolEntry {
            txid: TxHash([7u8; 32]),
            serialized_tx: Arc::new(SerializedTransaction::from(bytes)),
            raw_len,
            entry_height: Height(entry_height),
            entry_time: Some(1_700_000_000),
            first_seen_generation: 3,
        }
    }

    #[test]
    fn lightclient_raw_transaction_stamps_mempool_height_zero() {
        let bytes = vec![1, 2, 3, 4, 5];
        let entry = entry_with(bytes.clone(), 2_500_000);

        let raw = entry.to_lightclient_raw_transaction();

        // Unconfirmed txs use the height-0 "in mempool" sentinel on the wire,
        // never the protocol-internal entry_height.
        assert_eq!(raw.height, 0);
        assert_eq!(raw.data, bytes);
    }

    #[test]
    fn cost_applies_the_zip401_floor() {
        // Tiny tx: cost floored to the threshold.
        let tiny = entry_with(vec![0u8; 200], 1);
        assert_eq!(
            tiny.cost(),
            crate::config::MEMPOOL_TRANSACTION_COST_THRESHOLD
        );

        // Large tx: cost equals its serialized size.
        let large = entry_with(vec![0u8; 20_000], 1);
        assert_eq!(large.serialized_bytes().len(), 20_000);
        assert_eq!(large.cost(), 20_000);
    }
}
