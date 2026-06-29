//! Standalone, parallel-buildable finalised spend index: maps each transparent
//! outpoint to the txid of the transaction that consumed it.
//!
//! Proof of concept, gated behind the `outp_to_spend_index` feature. See
//! `docs/adr/0002-finalised-spend-index-parallel-build.md`.
//!
//! Pure, read-free build stages live here: **extract** spends from a block
//! batch and **collate** them into LMDB key order. The `MDB_APPEND` write and
//! the independent sync loop that drives these land in later slices. (Table-
//! level integrity over the entries is deferred; it is not MVP.)

use crate::chain_index::types::TransactionHash;
use crate::error::FinalisedStateError;
use crate::{IndexedBlock, Outpoint, TransparentCompactTx, ZainoVersionedSerde as _};

/// One transparent spend: the consumed outpoint paired with the txid of the
/// transaction that consumed it.
pub(super) type SpendRecord = (Outpoint, TransactionHash);

/// A spend entry encoded for storage: the LMDB key (the encoded outpoint) and
/// value (the bare 32-byte spending txid), ready for an `MDB_APPEND` load.
pub(super) type EncodedSpend = (Vec<u8>, [u8; 32]);

// ── Extract ──────────────────────────────────────────────────────────────────

/// Extracts every transparent spend recorded in `blocks`.
///
/// Pure and statically read-free: it is handed only block data — no database
/// and no validator handle — so a previous-output lookup is unrepresentable,
/// not merely discouraged. Each transparent input yields
/// `(prevout_outpoint, spending_txid)` directly, where the spending txid is the
/// containing transaction's own id; nothing outside the block stream is
/// consulted.
///
/// TODO (loop slice): add a buffer-filling variant that writes into a
/// caller-owned `&mut Vec<SpendRecord>`, once the per-worker loop exists to
/// reuse one allocation across batches (`Vec::clear` keeps capacity ⇒ ~zero
/// steady-state alloc, and the collator can sort that buffer in place).
/// Deferred until a real reusing caller anchors it: the extractor is dwarfed by
/// zebra block I/O and the sort, so buffer reuse only pays then.
pub(super) fn extract_spends(blocks: &[IndexedBlock]) -> Vec<SpendRecord> {
    blocks
        .iter()
        .flat_map(|block| block.transactions().iter())
        .flat_map(|tx| spends_in_transaction(*tx.txid(), tx.transparent()))
        .collect()
}

/// The spends contributed by one transaction: each outpoint it consumes paired
/// with `spending_txid`. The non-coinbase filtering lives in
/// [`TransparentCompactTx::spent_outpoints`]; this only attaches the spender.
fn spends_in_transaction(
    spending_txid: TransactionHash,
    transparent: &TransparentCompactTx,
) -> impl Iterator<Item = SpendRecord> + '_ {
    transparent
        .spent_outpoints()
        .map(move |outpoint| (outpoint, spending_txid))
}

// ── Collate ──────────────────────────────────────────────────────────────────

/// Encodes and sorts `records` into LMDB key order — byte-wise on the encoded
/// outpoint key, matching LMDB's default comparator — so the result can be
/// bulk-loaded with `MDB_APPEND`.
///
/// Spend keys are globally disjoint — each outpoint is spent at most once on a
/// chain — so this is a pure sort needing no cross-record reconciliation; a
/// duplicate key means corrupt input and is rejected.
pub(super) fn collate(records: &[SpendRecord]) -> Result<Vec<EncodedSpend>, FinalisedStateError> {
    let mut encoded = records
        .iter()
        .map(|(outpoint, spending_txid)| {
            Ok((outpoint.to_bytes()?, <[u8; 32]>::from(*spending_txid)))
        })
        .collect::<Result<Vec<EncodedSpend>, FinalisedStateError>>()?;

    encoded.sort_by(|a, b| a.0.cmp(&b.0));

    if let Some(duplicate) = encoded.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(FinalisedStateError::Custom(format!(
            "duplicate spend-index key during collation: {:?}",
            duplicate[0].0
        )));
    }

    Ok(encoded)
}

#[cfg(test)]
mod spends_in_transaction {
    use super::*;
    use crate::TxInCompact;

    /// Arbitrary fill byte for the spending transaction's txid, kept distinct
    /// from the prevout txids below so the two can't be confused.
    const SPENDER_TXID_BYTE: u8 = 9;

    fn txid(byte: u8) -> TransactionHash {
        TransactionHash::from([byte; 32])
    }

    #[test]
    fn skips_coinbase_input_keeps_real_spends() {
        let spender = txid(SPENDER_TXID_BYTE);
        let transparent = TransparentCompactTx::new(
            vec![
                TxInCompact::null_prevout(),    // coinbase input → contributes nothing
                TxInCompact::new([1u8; 32], 0), // spends output 0 of txid 0x01..
                TxInCompact::new([2u8; 32], 7), // spends output 7 of txid 0x02..
            ],
            vec![],
        );

        let records: Vec<SpendRecord> =
            super::spends_in_transaction(spender, &transparent).collect();

        assert_eq!(
            records,
            vec![
                (Outpoint::new([1u8; 32], 0), spender),
                (Outpoint::new([2u8; 32], 7), spender),
            ]
        );
    }
}

#[cfg(test)]
mod collate {
    use super::*;

    fn record(outpoint_byte: u8, index: u32) -> SpendRecord {
        (
            Outpoint::new([outpoint_byte; 32], index),
            TransactionHash::from([0xaau8; 32]),
        )
    }

    #[test]
    fn sorts_into_ascending_key_order() {
        let records = [record(3, 0), record(1, 0), record(2, 0)];
        let encoded = super::collate(&records).expect("disjoint keys collate cleanly");
        assert!(
            encoded.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "collated keys must be strictly ascending for MDB_APPEND",
        );
    }

    #[test]
    fn rejects_duplicate_keys() {
        let records = [record(1, 0), record(1, 0)];
        assert!(super::collate(&records).is_err());
    }
}
