//! The mempool entry — Zaino's foundational view of one unconfirmed transaction.
//!
//! The entry holds the full unmined transaction as the validator serialized it,
//! plus the protocol metadata the validator reported alongside it. It carries
//! **no** parsed or wire-shaped forms — no `Transaction`, no compact
//! transaction, no lightclient `RawTransaction`. Those conversions belong to the
//! boundary layer, and keeping them out is what lets this crate depend on no node
//! library at all.

use bytes::Bytes;
use zaino_primitives::types::{Height, TransactionId};

/// One unconfirmed transaction in Zaino's mempool read model.
///
/// Mirrors the fields of Zebra's `VerifiedUnminedTx` that Zaino serves or needs:
/// the transaction bytes, its size, and — crucially — `entry_height`, the chain
/// tip height when the transaction entered the mempool (Zebra's
/// `VerifiedUnminedTx.height`, zcashd's `nHeight`).
#[derive(Debug)]
pub struct MempoolEntry {
    /// The transaction's id.
    pub txid: TransactionId,

    /// Serialized transaction bytes, guaranteed to deserialize as a Zcash
    /// transaction (they came from the validator).
    ///
    /// Held as [`Bytes`] so serving the same transaction to many concurrent
    /// stream consumers costs a refcount bump each, not a copy each: the buffer
    /// is built once at ingest and shared all the way to the wire.
    pub serialized_tx: Bytes,

    /// Length of `serialized_tx` in bytes.
    ///
    /// `u64`, not `u32`: the cost accounting and the `getmempoolinfo` totals are
    /// `u64`, and a narrowing cast at ingest would silently wrap rather than
    /// bound anything.
    pub raw_len: u64,

    /// Chain tip height when the transaction entered the mempool.
    ///
    /// Sourced from the validator (`getrawmempool verbose`) so it matches the
    /// validator's own `nHeight`. This is protocol metadata, not a wire height.
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
        self.serialized_tx.as_ref()
    }

    /// The raw serialized transaction as a shared [`Bytes`] buffer.
    ///
    /// Cloning is a refcount bump, so prefer this over
    /// [`serialized_bytes`](Self::serialized_bytes)`.to_vec()` when handing the
    /// transaction to a wire type or a stream consumer.
    pub fn wire_bytes(&self) -> Bytes {
        self.serialized_tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(bytes: Vec<u8>, entry_height: u32) -> MempoolEntry {
        let raw_len = bytes.len() as u64;
        MempoolEntry {
            txid: TransactionId::from([7u8; 32]),
            serialized_tx: Bytes::from(bytes),
            raw_len,
            entry_height: Height::try_from(entry_height).expect("valid fixture height"),
            entry_time: Some(1_700_000_000),
            first_seen_generation: 3,
        }
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
