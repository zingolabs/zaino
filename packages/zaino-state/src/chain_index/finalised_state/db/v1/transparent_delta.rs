//! The single transparent created/spent delta of a block.
//!
//! "Enumerate a block's transparent inputs and outputs" was copied into every
//! consumer (the DB-write path, the txout-set accumulator, the address-history
//! index) and every direction (forward / reverse). This module owns that
//! enumeration once; each consumer drives off the resulting [`TransparentBlockDelta`]
//! rather than re-walking the transactions, and the reverse path is the same delta
//! applied inverted.

use super::*;

/// A block's transparent state transition: the outputs it creates and the outpoints
/// it spends, each tagged with the location of the transaction responsible.
pub(super) struct TransparentBlockDelta {
    /// Created outputs: `(outpoint, output, creating-tx location)`. Includes
    /// unspendable outputs; consumers that track only the UTXO set filter them with
    /// [`is_unspendable_tx_out`].
    // Consumed by the UTXO-cache wiring (maintain / seed / reorg), landing in stages.
    #[allow(dead_code)]
    pub(super) created: Vec<(Outpoint, TxOutCompact, TxLocation)>,
    /// Spent outpoints: `(spent outpoint, spending-tx location)`. Coinbase
    /// (null-prevout) inputs are skipped.
    pub(super) spent: Vec<(Outpoint, TxLocation)>,
}

/// Derives the transparent delta from a block's `(txid, transparent tx)` pairs.
///
/// This is the one place the "enumerate a block's transparent inputs and outputs"
/// loop lives. The result is a flat list per direction; consumers build whatever
/// projection they need (the write path's `spent` map, the accumulator's per-tx
/// counts, the cache's per-outpoint set) from it.
pub(super) fn block_transparent_delta(
    block_height: Height,
    transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
) -> Result<TransparentBlockDelta, FinalisedStateError> {
    let mut created = Vec::new();
    let mut spent = Vec::new();

    for (tx_index, (txid, transparent)) in transactions.iter().enumerate() {
        let Some(transparent) = transparent else {
            continue;
        };

        let tx_index = u16::try_from(tx_index).map_err(|_| {
            FinalisedStateError::Custom(format!(
                "transaction index {tx_index} does not fit into u16"
            ))
        })?;
        let location = TxLocation::new(block_height.0, tx_index);

        for (vout, output) in transparent.outputs().iter().enumerate() {
            let vout = u32::try_from(vout).map_err(|_| {
                FinalisedStateError::Custom("transparent output index does not fit into u32".into())
            })?;
            created.push((Outpoint::new(txid.0, vout), *output, location));
        }

        for input in transparent.inputs() {
            if input.is_null_prevout() {
                continue;
            }
            spent.push((
                Outpoint::new(*input.prevout_txid(), input.prevout_index()),
                location,
            ));
        }
    }

    Ok(TransparentBlockDelta { created, spent })
}

/// Builds the `outpoint -> spending-tx location` map the write path stores and the
/// accumulator consumes, shared by the forward write and the reverse delete.
///
/// No within-block duplicate-spend check: a duplicate transparent spend is a
/// double-spend, which consensus forbids and the source (zebra) already rejected, so
/// the check could only ever pass. (It was the in-memory sibling of the cross-block
/// double-spend read already removed from the accumulator path.)
pub(super) fn spent_map_from_delta(delta: &TransparentBlockDelta) -> HashMap<Outpoint, TxLocation> {
    delta.spent.iter().copied().collect()
}
