//! Standalone, parallel-buildable finalised spend index: maps each transparent
//! outpoint to the txid of the transaction that consumed it.
//!
//! Proof of concept, gated behind the `outp_to_spend_index` feature. See
//! `docs/adr/0002-finalised-spend-index-parallel-build.md`.
//!
//! This slice holds the **read-free extractor** — the pure first stage of the
//! build. The independent sync loop and the sorted-merge collator land in later
//! slices.

use crate::chain_index::types::TransactionHash;
use crate::{IndexedBlock, Outpoint, TransparentCompactTx};

/// One transparent spend: the consumed outpoint paired with the txid of the
/// transaction that consumed it.
pub(super) type SpendRecord = (Outpoint, TransactionHash);

/// Extracts every transparent spend recorded in `blocks`.
///
/// Pure and statically read-free: it is handed only block data — no database
/// and no validator handle — so a previous-output lookup is unrepresentable,
/// not merely discouraged. Each transparent input yields
/// `(prevout_outpoint, spending_txid)` directly, where the spending txid is the
/// containing transaction's own id; nothing outside the block stream is
/// consulted.
///
/// TODO (collator slice): replace with
/// `extract_spends_into(blocks, &mut Vec<SpendRecord>)` so the per-worker loop
/// reuses one buffer across batches — `clear()` retains capacity, dropping
/// steady-state allocation to ~zero, and the collator sorts that buffer in
/// place. Left as the simple collecting form for now: the extractor is dwarfed
/// by zebra block I/O and the sort, so buffer reuse only pays once wired into
/// the loop.
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
