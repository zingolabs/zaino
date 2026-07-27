//! Finalised txout-set accumulator (schema table #9): the finalised-state portion of
//! `gettxoutsetinfo`.

use super::*;
use crate::chain_index::finalised_state::finalised_source::v1::{
    ACCUMULATOR_BUILD_MAX_SHARDS, SPENT_SET_ENTRY_BYTES_ESTIMATE,
    TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY, TX_OUT_SET_INFO_ACCUMULATOR_KEY,
};
use crate::chain_index::finalised_state::finalised_source::FinalisedSource;
#[cfg(test)]
use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::source::BlockchainSource;
use crate::chain_index::types::db::metadata::{
    is_unspendable_tx_out, tx_out_set_entry_digest, FinalisedTxOutSetInfoAccumulator,
    ZAINO_TXOUTSET_ENTRY_LEN,
};

/// Direction of an accumulator update.
///
/// Forward (`Apply`) and reverse (`Reverse`) traverse the same shared helpers; the only
/// difference is the sign of every delta.
enum AccumulatorDirection {
    /// Applying a block forward (write path / migration backfill).
    Apply,
    /// Reversing a block (delete path).
    Reverse,
}

/// Applies a list of UTXO entries to the multiset commitment fields of the accumulator.
///
/// For each entry the digest is XORed into `hash_serialized` (XOR is self-inverse, so the same
/// call site works for both add and remove). The integer fields `total_zatoshis` and
/// `bytes_serialized` move in the direction selected by `adding`.
fn apply_tx_out_set_entries_delta(
    accumulator: &mut FinalisedTxOutSetInfoAccumulator,
    entries: &[(Outpoint, TxOutCompact)],
    adding: bool,
) -> Result<(), FinalisedStateError> {
    for (outpoint, out) in entries {
        let digest = tx_out_set_entry_digest(outpoint, out);
        for (dst, src) in accumulator.hash_serialized.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }

        if adding {
            accumulator.total_zatoshis = accumulator
                .total_zatoshis
                .checked_add(out.value())
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator total_zatoshis overflow".to_string(),
                    )
                })?;
            accumulator.bytes_serialized = accumulator
                .bytes_serialized
                .checked_add(ZAINO_TXOUTSET_ENTRY_LEN)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator bytes_serialized overflow".to_string(),
                    )
                })?;
        } else {
            accumulator.total_zatoshis = accumulator
                .total_zatoshis
                .checked_sub(out.value())
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator total_zatoshis underflow".to_string(),
                    )
                })?;
            accumulator.bytes_serialized = accumulator
                .bytes_serialized
                .checked_sub(ZAINO_TXOUTSET_ENTRY_LEN)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator bytes_serialized underflow".to_string(),
                    )
                })?;
        }
    }
    Ok(())
}

/// Applies the in-block portion of the accumulator update.
///
/// Handles both the bulk `transaction_outputs` delta and the per-tx 0↔>0 transition that
/// counts a same-block transaction as entering (apply) or leaving (reverse) the UTXO set.
/// The positional bound check (`spent_index >= created_count`) uses the *full* output
/// count via `spent_indices_by_tx`; the UTXO-set membership transition uses
/// `spendable_spent_count_by_tx` which excludes unspendable outputs.
fn apply_in_block_transitions(
    accumulator: &mut FinalisedTxOutSetInfoAccumulator,
    created_counts: &HashMap<TransactionHash, u32>,
    spendable_counts: &HashMap<TransactionHash, u32>,
    spent_indices_by_tx: &HashMap<TransactionHash, HashSet<u32>>,
    spendable_spent_count_by_tx: &HashMap<TransactionHash, u32>,
    spent_total_outputs: u64,
    direction: AccumulatorDirection,
) -> Result<(), FinalisedStateError> {
    let created_total = spendable_counts
        .values()
        .try_fold(0u64, |total, output_count| {
            total.checked_add(u64::from(*output_count)).ok_or_else(|| {
                FinalisedStateError::Custom(
                    "txout-set accumulator created output count overflow".to_string(),
                )
            })
        })?;

    accumulator.transaction_outputs = match direction {
        AccumulatorDirection::Apply => accumulator
            .transaction_outputs
            .checked_add(created_total)
            .and_then(|v| v.checked_sub(spent_total_outputs)),
        AccumulatorDirection::Reverse => accumulator
            .transaction_outputs
            .checked_sub(created_total)
            .and_then(|v| v.checked_add(spent_total_outputs)),
    }
    .ok_or_else(|| {
        FinalisedStateError::Custom(
            "txout-set accumulator transaction output count underflow or overflow".to_string(),
        )
    })?;

    for (transaction_hash, created_count) in created_counts {
        let spent_indices = spent_indices_by_tx.get(transaction_hash);

        if let Some(spent_indices) = spent_indices {
            for spent_index in spent_indices {
                if spent_index >= created_count {
                    return Err(FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be calculated: transaction {transaction_hash:?} spends same-block output index {spent_index}, but the transaction only has {created_count} transparent outputs"
                    )));
                }
            }
        }

        let spent_count = spendable_spent_count_by_tx
            .get(transaction_hash)
            .copied()
            .unwrap_or(0);

        let spendable_count = spendable_counts.get(transaction_hash).copied().unwrap_or(0);

        if spendable_count > spent_count {
            accumulator.transactions = match direction {
                AccumulatorDirection::Apply => accumulator.transactions.checked_add(1),
                AccumulatorDirection::Reverse => accumulator.transactions.checked_sub(1),
            }
            .ok_or_else(|| {
                FinalisedStateError::Custom(
                    "txout-set accumulator transaction count underflow or overflow".to_string(),
                )
            })?;
        }
    }

    Ok(())
}

/// Applies the per-entry deltas to `hash_serialized`, `bytes_serialized` and
/// `total_zatoshis`.
///
/// Both `created_entries` and `spent_entries` must already be filtered to exclude
/// unspendable outputs — they were never in the UTXO set.
fn apply_entry_deltas(
    accumulator: &mut FinalisedTxOutSetInfoAccumulator,
    created_entries: &[(Outpoint, TxOutCompact)],
    spent_entries: &[(Outpoint, TxOutCompact)],
    direction: AccumulatorDirection,
) -> Result<(), FinalisedStateError> {
    let (created_adding, spent_adding) = match direction {
        AccumulatorDirection::Apply => (true, false),
        AccumulatorDirection::Reverse => (false, true),
    };

    apply_tx_out_set_entries_delta(accumulator, created_entries, created_adding)?;
    apply_tx_out_set_entries_delta(accumulator, spent_entries, spent_adding)?;

    Ok(())
}

/// Builds the per-transaction output count maps used by the accumulator helpers.
///
/// Returns `(total_count_by_tx, spendable_count_by_tx)`:
/// - `total_count_by_tx` counts every transparent output and is used for positional
///   consensus bound checks.
/// - `spendable_count_by_tx` excludes provably-unspendable outputs (see
///   [`is_unspendable_tx_out`]) and is what drives UTXO-set deltas.
#[allow(clippy::type_complexity)]
fn index_created_outputs(
    transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
) -> Result<(HashMap<TransactionHash, u32>, HashMap<TransactionHash, u32>), FinalisedStateError> {
    let mut total_by_tx: HashMap<TransactionHash, u32> = HashMap::with_capacity(transactions.len());
    let mut spendable_by_tx: HashMap<TransactionHash, u32> =
        HashMap::with_capacity(transactions.len());

    for (transaction_hash, transparent_transaction) in transactions {
        let (total, spendable) = transparent_transaction
            .as_ref()
            .map(|tx| {
                let total = tx.outputs().len();
                let spendable = tx
                    .outputs()
                    .iter()
                    .filter(|o| !is_unspendable_tx_out(o))
                    .count();
                (total, spendable)
            })
            .unwrap_or((0, 0));

        let total = u32::try_from(total).map_err(|_| {
            FinalisedStateError::Custom(
                "txout-set accumulator cannot be calculated: transparent output count does not fit into u32"
                    .to_string(),
            )
        })?;
        let spendable = u32::try_from(spendable).map_err(|_| {
            FinalisedStateError::Custom(
                "txout-set accumulator cannot be calculated: spendable output count does not fit into u32"
                    .to_string(),
            )
        })?;

        if total_by_tx.insert(*transaction_hash, total).is_some() {
            return Err(FinalisedStateError::Custom(format!(
                "txout-set accumulator cannot be calculated: duplicate transaction hash in block: {transaction_hash:?}"
            )));
        }
        spendable_by_tx.insert(*transaction_hash, spendable);
    }

    Ok((total_by_tx, spendable_by_tx))
}

/// Groups a block's spent outpoints by the transaction they spend from.
///
/// Returns `(spent_indices_by_tx, spent_outpoints_with_locations)`. The forward path
/// projects out just the outpoints; the reverse path needs the locations to verify the
/// spent index points to this block.
#[allow(clippy::type_complexity)]
fn index_spent_outpoints(
    spent_map: &HashMap<Outpoint, TxLocation>,
) -> Result<
    (
        HashMap<TransactionHash, HashSet<u32>>,
        Vec<(Outpoint, TxLocation)>,
    ),
    FinalisedStateError,
> {
    let mut by_tx: HashMap<TransactionHash, HashSet<u32>> = HashMap::new();
    let mut outpoints = Vec::with_capacity(spent_map.len());

    for (outpoint, tx_location) in spent_map.iter() {
        let previous_transaction_hash = TransactionHash::from(*outpoint.prev_txid());

        let inserted = by_tx
            .entry(previous_transaction_hash)
            .or_default()
            .insert(outpoint.prev_index());

        if !inserted {
            return Err(FinalisedStateError::Custom(format!(
                "txout-set accumulator cannot be calculated: duplicate transparent spend for outpoint {outpoint:?}"
            )));
        }

        outpoints.push((*outpoint, *tx_location));
    }

    Ok((by_tx, outpoints))
}

/// Read, maintenance, and bulk (re)build of the finalised txout-set accumulator (schema table #9).
///
/// Groups the accumulator's read / write-path-delta / rebuild surface.
impl DbV1 {
    /// Resolves each spent outpoint to its previous [`TxOutCompact`].
    ///
    /// Same-block spends are resolved from the in-block `transactions` slice via the
    /// `txid_to_block_index` map. Prior-block spends are resolved via
    /// [`DbV1::get_previous_output_blocking`] inside a `block_in_place` to honour the read/write
    /// boundary requirements documented on that method.
    fn resolve_spent_outpoints_for_set_info(
        &self,
        spent_map: &HashMap<Outpoint, TxLocation>,
        txid_to_block_index: &HashMap<TransactionHash, usize>,
        transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
    ) -> Result<Vec<(Outpoint, TxOutCompact)>, FinalisedStateError> {
        let mut resolved = Vec::with_capacity(spent_map.len());

        for outpoint in spent_map.keys().copied() {
            let prev_txid = TransactionHash::from(*outpoint.prev_txid());
            let prev_index = outpoint.prev_index() as usize;

            let prev_out = if let Some(block_tx_index) = txid_to_block_index.get(&prev_txid) {
                let tx = transactions[*block_tx_index].1.as_ref().ok_or_else(|| {
                    FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be calculated: same-block spend of {prev_txid:?} has no transparent transaction data"
                    ))
                })?;
                *tx.outputs().get(prev_index).ok_or_else(|| {
                    FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be calculated: same-block spend of {prev_txid:?} index {prev_index} out of range"
                    ))
                })?
            } else {
                tokio::task::block_in_place(|| self.get_previous_output_blocking(outpoint))?
            };

            resolved.push((outpoint, prev_out));
        }

        Ok(resolved)
    }

    /// Applies the prior-block portion of the accumulator update.
    ///
    /// For every transaction spent from by this block that was *not* created in this block,
    /// loads its previous transparent transaction, checks the positional bound, and decides
    /// whether the block drains every remaining spendable output of that prior transaction.
    /// If so, the prior tx leaves (apply) or re-enters (reverse) the UTXO set.
    async fn apply_prior_block_transitions(
        &self,
        accumulator: &mut FinalisedTxOutSetInfoAccumulator,
        spent_indices_by_tx: &HashMap<TransactionHash, HashSet<u32>>,
        created_in_block: &HashMap<TransactionHash, u32>,
        direction: AccumulatorDirection,
    ) -> Result<(), FinalisedStateError> {
        for (transaction_hash, spent_indices) in spent_indices_by_tx {
            if created_in_block.contains_key(transaction_hash) {
                continue;
            }

            let Some(transaction_location) =
                <Self as BlockCoreExt>::get_tx_location(self, transaction_hash).await?
            else {
                return Err(FinalisedStateError::Custom(format!(
                    "txout-set accumulator cannot be calculated: spent transaction {transaction_hash:?} is missing from the txid index"
                )));
            };

            let Some(transparent_transaction) =
                <Self as BlockTransparentExt>::get_transparent(self, transaction_location).await?
            else {
                return Err(FinalisedStateError::Custom(format!(
                    "txout-set accumulator cannot be calculated: spent transaction {transaction_hash:?} has no transparent transaction data"
                )));
            };

            let previous_output_count = u32::try_from(transparent_transaction.outputs().len())
                .map_err(|_| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator previous transparent output count does not fit into u32"
                            .to_string(),
                    )
                })?;

            for spent_index in spent_indices {
                if *spent_index >= previous_output_count {
                    return Err(FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be calculated: transaction {transaction_hash:?} spends output index {spent_index}, but the previous transaction only has {previous_output_count} transparent outputs"
                    )));
                }
            }

            // Spendable outputs of the prior tx that this block did not spend.
            let mut remaining_outpoints = Vec::new();
            for (output_index, prev_output) in transparent_transaction.outputs().iter().enumerate()
            {
                let output_index = output_index as u32;
                if is_unspendable_tx_out(prev_output) {
                    continue;
                }
                if spent_indices.contains(&output_index) {
                    continue;
                }
                remaining_outpoints.push(Outpoint::new(transaction_hash.0, output_index));
            }

            // The prior tx leaves the UTXO set (apply) / re-enters it (reverse) when this block
            // accounts for every spendable output that was still unspent before this block.
            let leaves_set = if remaining_outpoints.is_empty() {
                true
            } else {
                let remaining_spenders =
                    <Self as TransparentHistExt>::get_outpoint_spenders(self, remaining_outpoints)
                        .await?;
                !remaining_spenders.into_iter().any(|s| s.is_none())
            };

            if leaves_set {
                accumulator.transactions = match direction {
                    AccumulatorDirection::Apply => accumulator.transactions.checked_sub(1),
                    AccumulatorDirection::Reverse => accumulator.transactions.checked_add(1),
                }
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator transaction count underflow or overflow".to_string(),
                    )
                })?;
            }
        }

        Ok(())
    }

    /// Resolves and filters the created and spent entry lists for accumulator updates.
    ///
    /// Created entries are collected from the block's transactions, excluding unspendable
    /// outputs. Spent entries are resolved (same-block from `transactions`, prior-block from
    /// the database) and likewise filtered to exclude unspendable outputs.
    ///
    /// Returns `(created_entries, spent_entries, spendable_spent_count_by_tx)`.
    /// `spendable_spent_count_by_tx` counts only spendable same-block spends per source tx.
    #[allow(clippy::type_complexity)]
    fn build_entry_data(
        &self,
        transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
        spent_map: &HashMap<Outpoint, TxLocation>,
    ) -> Result<
        (
            Vec<(Outpoint, TxOutCompact)>,
            Vec<(Outpoint, TxOutCompact)>,
            HashMap<TransactionHash, u32>,
        ),
        FinalisedStateError,
    > {
        let mut created_entries: Vec<(Outpoint, TxOutCompact)> = Vec::new();
        let mut txid_to_block_index: HashMap<TransactionHash, usize> =
            HashMap::with_capacity(transactions.len());

        for (transaction_index, (transaction_hash, transparent_transaction)) in
            transactions.iter().enumerate()
        {
            txid_to_block_index.insert(*transaction_hash, transaction_index);

            let Some(transparent_transaction) = transparent_transaction.as_ref() else {
                continue;
            };

            for (output_index, output) in transparent_transaction.outputs().iter().enumerate() {
                if is_unspendable_tx_out(output) {
                    continue;
                }
                let outpoint = Outpoint::new(transaction_hash.0, output_index as u32);
                created_entries.push((outpoint, *output));
            }
        }

        let resolved = self.resolve_spent_outpoints_for_set_info(
            spent_map,
            &txid_to_block_index,
            transactions,
        )?;

        let mut spent_entries = Vec::with_capacity(resolved.len());
        let mut spendable_spent_count_by_tx: HashMap<TransactionHash, u32> = HashMap::new();

        for (outpoint, out) in resolved {
            if is_unspendable_tx_out(&out) {
                continue;
            }
            let prev_txid = TransactionHash::from(*outpoint.prev_txid());
            *spendable_spent_count_by_tx.entry(prev_txid).or_default() += 1;
            spent_entries.push((outpoint, out));
        }

        Ok((created_entries, spent_entries, spendable_spent_count_by_tx))
    }
    /// Provides access to the finalised txout-set accumulator DB table.
    pub(crate) fn tx_out_set_info_accumulator_db(&self) -> Database {
        self.tx_out_set_info_accumulator
    }

    /// Returns the finalised-state txout-set accumulator.
    ///
    /// This reads the singleton accumulator entry. It does not compute or repair the accumulator;
    /// accumulator creation, backfill, and updates are handled by migrations and write paths.
    pub(super) async fn get_tx_out_set_info_accumulator(
        &self,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let transaction = self.env.begin_ro_txn()?;

            let raw_accumulator = match transaction.get(
                self.tx_out_set_info_accumulator,
                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
            ) {
                Ok(value) => value,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "finalised txout-set accumulator missing from database".to_string(),
                    ));
                }
                Err(error) => return Err(FinalisedStateError::LmdbError(error)),
            };

            let accumulator_entry =
                StoredEntryFixed::<FinalisedTxOutSetInfoAccumulator>::from_bytes(raw_accumulator)
                    .map_err(|error| {
                    FinalisedStateError::Custom(format!(
                        "txout-set accumulator decode error: {error}"
                    ))
                })?;

            if !accumulator_entry.verify(TX_OUT_SET_INFO_ACCUMULATOR_KEY) {
                return Err(FinalisedStateError::Custom(
                    "txout-set accumulator checksum mismatch".to_string(),
                ));
            }

            Ok(accumulator_entry.item)
        })
    }

    /// Calculates the finalised txout-set accumulator after applying the block currently being written.
    ///
    /// This method uses the data already built by `write_block`:
    /// - `transactions`: block-local `(transaction_hash, transparent_transaction)` pairs.
    ///   Pairing is established at construction in `write_block` (both halves come from the
    ///   same `tx`), so the accumulator never has to trust index alignment between two
    ///   parallel slices.
    /// - `spent_map`: distinct transparent outpoints spent by this block.
    ///
    /// Missing accumulator data is only valid for a completely empty database before writing genesis.
    /// In every other case, a missing accumulator is treated as database corruption / failed migration.
    ///
    /// The returned accumulator must be written inside the same LMDB write transaction as the block.
    pub(crate) async fn calculate_tx_out_set_info_accumulator_after_block(
        &self,
        block_height: Height,
        transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
        spent_map: &HashMap<Outpoint, TxLocation>,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        // Load the existing accumulator. Only a fresh empty DB writing genesis may start from zero.
        let mut accumulator =
            match <Self as TransparentHistExt>::get_tx_out_set_info_accumulator(self).await {
                Ok(accumulator) => accumulator,
                Err(FinalisedStateError::DataUnavailable(_)) => {
                    let current_tip = self.tip_height().await?;

                    if current_tip.is_none() && block_height == GENESIS_HEIGHT {
                        FinalisedTxOutSetInfoAccumulator::empty()
                    } else {
                        return Err(FinalisedStateError::Custom(
                            "txout-set accumulator missing from non-empty database".to_string(),
                        ));
                    }
                }
                Err(error) => return Err(error),
            };

        let (created_counts, spendable_counts) = index_created_outputs(transactions)?;
        let (spent_indices_by_tx, spent_outpoints) = index_spent_outpoints(spent_map)?;

        // Forward-direction validation: outpoints spent by this block must not already be
        // spent in finalised state (same-block spends are not in the finalised spent table
        // yet and are skipped).
        if !spent_outpoints.is_empty() {
            let outpoints: Vec<Outpoint> = spent_outpoints.iter().map(|(o, _)| *o).collect();
            let existing_spenders =
                <Self as TransparentHistExt>::get_outpoint_spenders(self, outpoints.clone())
                    .await?;
            for (spent_outpoint, existing_spender) in outpoints.iter().zip(existing_spenders) {
                if created_counts.contains_key(&TransactionHash::from(*spent_outpoint.prev_txid()))
                {
                    continue;
                }
                if let Some(existing_spender) = existing_spender {
                    return Err(FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be calculated: block spends already-spent outpoint {spent_outpoint:?}; existing spender is {existing_spender:?}"
                    )));
                }
            }
        }

        let (created_entries, spent_entries, spendable_spent_count_by_tx) =
            self.build_entry_data(transactions, spent_map)?;

        let spent_total_outputs = u64::try_from(spent_entries.len()).map_err(|_| {
            FinalisedStateError::Custom(
                "txout-set accumulator spent output count does not fit into u64".to_string(),
            )
        })?;

        apply_in_block_transitions(
            &mut accumulator,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            spent_total_outputs,
            AccumulatorDirection::Apply,
        )?;
        self.apply_prior_block_transitions(
            &mut accumulator,
            &spent_indices_by_tx,
            &created_counts,
            AccumulatorDirection::Apply,
        )
        .await?;
        apply_entry_deltas(
            &mut accumulator,
            &created_entries,
            &spent_entries,
            AccumulatorDirection::Apply,
        )?;

        Ok(accumulator)
    }

    /// Calculates the finalised txout-set accumulator after deleting the tip block.
    ///
    /// This is the exact inverse of `calculate_tx_out_set_info_accumulator_after_block`.
    ///
    /// The database must still contain the block being deleted when this method is called.
    /// The returned accumulator must be written inside the same LMDB transaction that deletes the block.
    pub(crate) async fn calculate_tx_out_set_info_accumulator_after_delete_block(
        &self,
        transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
        spent_map: &HashMap<Outpoint, TxLocation>,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        let mut accumulator =
            match <Self as TransparentHistExt>::get_tx_out_set_info_accumulator(self).await {
                Ok(accumulator) => accumulator,
                Err(FinalisedStateError::DataUnavailable(_)) => {
                    return Err(FinalisedStateError::Custom(
                        "txout-set accumulator missing while deleting block".to_string(),
                    ));
                }
                Err(error) => return Err(error),
            };

        let (created_counts, spendable_counts) = index_created_outputs(transactions)?;
        let (spent_indices_by_tx, spent_outpoints) = index_spent_outpoints(spent_map)?;

        // Reverse-direction validation: every spent outpoint from this block must be in the
        // finalised spent index and must point to this block's TxLocation.
        if !spent_outpoints.is_empty() {
            let outpoints: Vec<Outpoint> = spent_outpoints.iter().map(|(o, _)| *o).collect();
            let existing_spenders =
                <Self as TransparentHistExt>::get_outpoint_spenders(self, outpoints).await?;
            for ((spent_outpoint, expected_tx_location), existing_spender) in
                spent_outpoints.iter().zip(existing_spenders)
            {
                let Some(existing_spender) = existing_spender else {
                    return Err(FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be reversed: spent index missing outpoint {spent_outpoint:?}"
                    )));
                };
                if existing_spender != *expected_tx_location {
                    return Err(FinalisedStateError::Custom(format!(
                        "txout-set accumulator cannot be reversed: outpoint {spent_outpoint:?} is spent by {existing_spender:?}, expected {expected_tx_location:?}"
                    )));
                }
            }
        }

        let (created_entries, spent_entries, spendable_spent_count_by_tx) =
            self.build_entry_data(transactions, spent_map)?;

        let spent_total_outputs = u64::try_from(spent_entries.len()).map_err(|_| {
            FinalisedStateError::Custom(
                "txout-set accumulator spent output count does not fit into u64".to_string(),
            )
        })?;

        apply_in_block_transitions(
            &mut accumulator,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            spent_total_outputs,
            AccumulatorDirection::Reverse,
        )?;
        self.apply_prior_block_transitions(
            &mut accumulator,
            &spent_indices_by_tx,
            &created_counts,
            AccumulatorDirection::Reverse,
        )
        .await?;
        apply_entry_deltas(
            &mut accumulator,
            &created_entries,
            &spent_entries,
            AccumulatorDirection::Reverse,
        )?;

        Ok(accumulator)
    }

    /// Persists the txout-set accumulator singleton (schema table #9) into `txn`; the caller
    /// commits and syncs. The single encoder of the singleton — shared by the write/delete paths
    /// and the bulk-rebuild / incremental-update builders so the on-disk encoding lives in one
    /// place.
    pub(super) fn put_tx_out_set_accumulator(
        &self,
        txn: &mut lmdb::RwTransaction,
        accumulator: FinalisedTxOutSetInfoAccumulator,
    ) -> Result<(), FinalisedStateError> {
        let entry = StoredEntryFixed::new(TX_OUT_SET_INFO_ACCUMULATOR_KEY, accumulator);
        txn.put(
            self.tx_out_set_info_accumulator,
            &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
            &entry.to_bytes()?,
            WriteFlags::empty(),
        )?;
        Ok(())
    }

    /// Advances the accumulator freshness watermark to `height` in `txn`; the caller commits.
    pub(super) fn put_tx_out_set_accumulator_watermark(
        &self,
        txn: &mut lmdb::RwTransaction,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        let watermark = StoredEntryFixed::new(TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY, height);
        txn.put(
            self.metadata,
            &TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
            &watermark.to_bytes()?,
            WriteFlags::empty(),
        )?;
        Ok(())
    }

    /// Computes the accumulator delta for a just-written block — but only on the single-block
    /// append path (`update_tx_out_set`); the bulk-sync path defers maintenance and rebuilds once
    /// at the tip. `None` means "not maintained on this write".
    pub(super) async fn maybe_calculate_tx_out_set_info_accumulator_after_block(
        &self,
        update_tx_out_set: bool,
        block_height: Height,
        transactions: &[(TransactionHash, Option<TransparentCompactTx>)],
        spent_map: &HashMap<Outpoint, TxLocation>,
    ) -> Result<Option<FinalisedTxOutSetInfoAccumulator>, FinalisedStateError> {
        if update_tx_out_set {
            Ok(Some(
                self.calculate_tx_out_set_info_accumulator_after_block(
                    block_height,
                    transactions,
                    spent_map,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Persists a maintained accumulator and advances its freshness watermark to `height` in `txn`,
    /// if one was computed on this write (the single-block append path). A no-op for `None`.
    pub(super) fn put_maintained_tx_out_set_accumulator(
        &self,
        txn: &mut lmdb::RwTransaction,
        accumulator: Option<FinalisedTxOutSetInfoAccumulator>,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        if let Some(accumulator) = accumulator {
            self.put_tx_out_set_accumulator(txn, accumulator)?;
            self.put_tx_out_set_accumulator_watermark(txn, height)?;
        }
        Ok(())
    }

    /// Brings the deferred txout-set accumulator up to `height` after a bulk write run: the cheap
    /// incremental delta when the watermark is within [`ACCUMULATOR_INCREMENTAL_MAX_GAP`] of the
    /// tip, otherwise a full from-genesis rebuild. Extracted from `write_blocks_to_height`.
    pub(super) async fn advance_tx_out_set_accumulator_to_tip(
        &self,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        match self.read_tx_out_set_accumulator_built_height().await? {
            Some(built) if built.0 >= height.0 => {}
            Some(built) if height.0.saturating_sub(built.0) <= ACCUMULATOR_INCREMENTAL_MAX_GAP => {
                info!(
                    "write_blocks_to_height: updating txout-set accumulator {}..={}",
                    built.0 + 1,
                    height.0
                );
                self.update_tx_out_set_accumulator_for_range(built, height)
                    .await?;
            }
            _ => {
                info!(
                    "write_blocks_to_height: rebuilding txout-set accumulator to height {}",
                    height.0
                );
                self.rebuild_tx_out_set_accumulator().await?;
            }
        }
        Ok(())
    }

    // *** Bulk txout-set accumulator builder ***
    //
    // Replaces the per-block, random-read accumulator maintenance that dominated sync time at
    // sandblast height. The accumulator over the UTXO set at the current tip is recomputed from
    // scratch with (almost entirely) sequential scans, exploiting the fact that the
    // `hash_serialized` field is an XOR multiset commitment: an output created and later spent is
    // XORed in then out and cancels, so the live set is exactly the created-and-not-spent outputs.

    /// Rebuilds the finalised txout-set accumulator to the current db tip and persists it.
    ///
    /// Atomically writes the recomputed accumulator singleton and the
    /// [`TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY`] watermark, then forces a durability sync. This is
    /// idempotent — it never trusts a pre-existing accumulator — so it is safe to call after an
    /// interrupted sync, and is reused by the v1.2 migration's accumulator stage.
    pub(crate) async fn rebuild_tx_out_set_accumulator(&self) -> Result<(), FinalisedStateError> {
        let Some(db_tip) = self.tip_height().await? else {
            // Empty database: nothing to build.
            return Ok(());
        };

        // Bound the rebuild's peak RAM to the dedicated accumulator-rebuild budget by sharding the
        // in-memory spent set; on hosts where the whole set fits this resolves to a single optimal
        // pass. This budget is intentionally *separate* from the bulk-sync write-batch budget so the
        // two operations cannot inflate each other's peak memory.
        let budget = (self
            .config
            .storage
            .database
            .accumulator_rebuild_memory_size
            .to_byte_count() as u64)
            .max(1);
        // Logged before any cursor work: choosing the initial shard count scans the `spent` table,
        // and a native LMDB abort there (a torn DB) is otherwise an unattributable stderr-only crash.
        info!(
            "txout-set accumulator rebuild to height {}: sizing shards (~{budget} byte budget)",
            db_tip.0
        );
        // `shards` is the *initial* partition (a good first guess from the memory estimate);
        // `max_spent_entries` is the *hard* per-shard cap the builder enforces during the spent
        // load, bisecting any shard that would exceed it. So the estimate only affects how many
        // passes we make, never whether we stay within budget.
        let shards = self.accumulator_build_shards(budget)?;
        let max_spent_entries = (budget / SPENT_SET_ENTRY_BYTES_ESTIMATE).max(1);
        info!(
            "txout-set accumulator rebuild to height {}: {shards} initial shard(s), \
             ≤{max_spent_entries} spent outpoints/shard, ~{budget} byte budget",
            db_tip.0
        );

        tokio::task::block_in_place(|| {
            let accumulator =
                self.build_tx_out_set_accumulator_blocking(db_tip, shards, max_spent_entries)?;

            let mut txn = self.env.begin_rw_txn()?;

            self.put_tx_out_set_accumulator(&mut txn, accumulator)?;
            self.put_tx_out_set_accumulator_watermark(&mut txn, db_tip)?;

            txn.commit()?;
            self.env.sync(true)?;

            Ok::<_, FinalisedStateError>(())
        })
    }

    /// Chooses the number of [`DbV1::build_tx_out_set_accumulator_blocking`] shards so the per-shard
    /// in-memory spent set stays within `budget_bytes`.
    ///
    /// `shards = ceil(estimated_spent_set_bytes / budget)`, clamped to
    /// `1..=ACCUMULATOR_BUILD_MAX_SHARDS`. Hosts with enough RAM for the whole spent set get a
    /// single optimal pass; constrained hosts scale up.
    ///
    /// This is only the *initial* partition handed to
    /// [`DbV1::build_tx_out_set_accumulator_blocking`] — a good first guess that minimises passes.
    /// The actual memory bound is enforced separately and strictly by that builder (it bisects any
    /// shard whose spent set would exceed the cap), so an inaccurate estimate here only changes how
    /// many passes are made, never whether the rebuild stays within budget.
    pub(crate) fn accumulator_build_shards(
        &self,
        budget_bytes: u64,
    ) -> Result<u16, FinalisedStateError> {
        let budget = budget_bytes.max(1);
        // Only count the spent set up to the point where we'd hit the shard cap anyway — past it
        // the exact size cannot change the decision, so the count pass is itself bounded.
        let max_useful_entries = (ACCUMULATOR_BUILD_MAX_SHARDS as u64).saturating_mul(budget)
            / SPENT_SET_ENTRY_BYTES_ESTIMATE.max(1);
        let needed = self
            .estimate_spent_set_bytes(max_useful_entries)?
            .div_ceil(budget);
        Ok(needed.clamp(1, ACCUMULATOR_BUILD_MAX_SHARDS as u64) as u16)
    }

    /// Estimates the in-RAM bytes the rebuild's single-shard spent set would occupy: the `spent`
    /// table's entry count times [`SPENT_SET_ENTRY_BYTES_ESTIMATE`].
    ///
    /// Counts via a sequential cursor scan (the safe `lmdb` API exposes no per-sub-DB stat), stopping
    /// early once `max_useful_entries` is reached. The builder's per-shard loads are range-seeks that
    /// together touch `spent` once in total, so this single extra count pass roughly doubles the
    /// spent-table reads — bounded, and dwarfed by the chain-length block scans.
    fn estimate_spent_set_bytes(
        &self,
        max_useful_entries: u64,
    ) -> Result<u64, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let cursor = txn.open_ro_cursor(self.spent)?;

            // Explicit cursor walk rather than `Cursor::iter`: that iterator `debug_assert!`-panics
            // on any non-`NotFound` LMDB error in debug builds and silently ends the scan in release
            // (truncating the count). Here a real LMDB error propagates cleanly and the count is only
            // ever ended by a genuine end-of-table `NotFound`. Counting allocates nothing (the cursor
            // yields references into the mmap), so this is O(1) heap regardless of table size.
            let mut count: u64 = 0;
            let mut op = lmdb_sys::MDB_FIRST;
            loop {
                match cursor.get(None, None, op) {
                    Ok(_) => {
                        count += 1;
                        if count >= max_useful_entries {
                            break;
                        }
                    }
                    Err(lmdb::Error::NotFound) => break,
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                }
                op = lmdb_sys::MDB_NEXT;
            }

            Ok(count.saturating_mul(SPENT_SET_ENTRY_BYTES_ESTIMATE))
        })
    }

    /// Computes the finalised txout-set accumulator over the UTXO set at `db_tip`.
    ///
    /// Strategy (per shard): **range-seek** the `spent` table over the shard's contiguous key range
    /// to collect the spent outpoints whose creating txid falls in the shard, then scan the block
    /// `transparent` + `txids` tables in ascending height order, adding every spendable output that
    /// is not in that spent set. The `transactions` count is derived locally per transaction (all of
    /// a tx's outputs live in one height entry). Sharding bounds the in-memory spent set; partials
    /// recombine exactly.
    ///
    /// The range-seek matters: `spent` keys are sorted and the version tag is constant, so a shard's
    /// first-byte range `[lo, hi)` is one contiguous key range. Seeking to it (rather than scanning
    /// the whole table and filtering) makes the total spent-table work O(N) across all shards instead
    /// of O(shards·N) — at maximal sharding (256) that is the difference between one sweep and 256
    /// full-table sweeps, which on a cgroup-limited host is also a page-cache pressure / OOM risk.
    ///
    /// WARNING: blocking — call from a blocking context. Builds to `db_tip` only (the spent table
    /// is assumed to cover spends up to the same tip).
    pub(crate) fn build_tx_out_set_accumulator_blocking(
        &self,
        db_tip: Height,
        shards: u16,
        max_spent_entries: u64,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        let shards = shards.max(1) as usize;
        let max_spent_entries = max_spent_entries.max(1);
        let mut total = FinalisedTxOutSetInfoAccumulator::empty();

        // Work-list of creating-txid first-byte ranges `[lo, hi)` still to process, seeded with
        // `shards` equal ranges (the memory estimate's initial partition — a good first guess that
        // avoids splitting down from the whole space). A range whose spent set would exceed
        // `max_spent_entries` is bisected and retried, so every shard actually loaded holds a *hard*
        // ≤ `max_spent_entries` outpoints — the bound holds regardless of the estimate's accuracy or
        // the txid distribution. The ranges stay a disjoint cover of `[0, 256)`, and XOR/sum
        // recombination is order-independent, so the result is identical for any partition.
        let mut pending: Vec<(u16, u16)> = (0..shards)
            .map(|shard| {
                (
                    (shard * 256 / shards) as u16,
                    ((shard + 1) * 256 / shards) as u16,
                )
            })
            .collect();

        while let Some((lo, hi)) = pending.pop() {
            match self.accumulate_tx_out_set_shard_blocking(db_tip, lo, hi, max_spent_entries)? {
                Some(shard_acc) => total
                    .combine(&shard_acc)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?,
                None => {
                    if hi - lo <= 1 {
                        // A single creating-txid first-byte value cannot be split further. Fail with
                        // an actionable error rather than OOM-ing: the spent outpoints sharing this
                        // first byte do not fit the configured budget.
                        return Err(FinalisedStateError::Custom(format!(
                            "txout-set accumulator: spent shard for creating-txid first-byte {lo} \
                             exceeds the per-shard budget ({max_spent_entries} outpoints) and cannot \
                             be split further; raise accumulator_rebuild_memory_size"
                        )));
                    }
                    let mid = lo + (hi - lo) / 2;
                    pending.push((lo, mid));
                    pending.push((mid, hi));
                }
            }
        }

        Ok(total)
    }

    /// Builds the accumulator partial for the creating-txid first-byte range `[lo, hi)`, or returns
    /// `Ok(None)` if the shard's in-memory spent set would exceed `max_spent_entries` outpoints —
    /// the signal for the caller to bisect the range and retry.
    ///
    /// Aborting *during* the spent load (before the limit is exceeded, dropping the partial set) is
    /// what makes the per-shard memory a hard cap rather than an estimate. An aborted shard does no
    /// block-table work, so a split wastes only a bounded partial spent scan.
    fn accumulate_tx_out_set_shard_blocking(
        &self,
        db_tip: Height,
        lo: u16,
        hi: u16,
        max_spent_entries: u64,
    ) -> Result<Option<FinalisedTxOutSetInfoAccumulator>, FinalisedStateError> {
        let in_shard = |first_byte: u8| -> bool {
            let b = first_byte as u16;
            b >= lo && b < hi
        };

        // One read snapshot for the whole shard pass (subsumes the per-lookup RO-txn churn the old
        // per-block path incurred).
        let txn = self.env.begin_ro_txn()?;

        // (1) Spent outpoints in this shard. The `spent` key is `Outpoint::to_bytes()` =
        //     `[version tag][32-byte prev_txid][4-byte index]`, so the prev-txid's first byte
        //     (which equals the creating txid's first byte) is at index 1. Because the keys are
        //     sorted and the version tag is constant, the shard's keys form one contiguous range;
        //     seek to its start and stop once we pass `hi` rather than scanning the whole table.
        let mut spent_set: HashSet<Box<[u8]>> = HashSet::new();
        {
            let mut shard_start_outpoint = [0u8; 32];
            shard_start_outpoint[0] = lo as u8;
            let shard_lower_bound = Outpoint::new(shard_start_outpoint, 0).to_bytes()?;

            // Seek to the first spent key >= the shard's lower bound, then walk forward with
            // `MDB_NEXT` until the first byte leaves `[lo, hi)`. `MDB_SET_RANGE` returns `NotFound`
            // when no key is at/after the bound (empty table, or a shard past the largest key) —
            // that is simply an empty shard. (`Cursor::iter_from` is unusable here: it `unwrap()`s
            // that `NotFound` and would panic.)
            let cursor = txn.open_ro_cursor(self.spent)?;
            let mut next = match cursor.get(
                Some(shard_lower_bound.as_slice()),
                None,
                lmdb_sys::MDB_SET_RANGE,
            ) {
                Ok((key, _value)) => key,
                Err(lmdb::Error::NotFound) => None,
                Err(error) => return Err(FinalisedStateError::LmdbError(error)),
            };
            while let Some(key_bytes) = next {
                if key_bytes.len() >= 2 {
                    // Sorted keys: once the first byte reaches `hi` we are past this shard.
                    if key_bytes[1] as u16 >= hi {
                        break;
                    }
                    // The seek guarantees `>= lo` for well-formed keys; re-check defensively so a
                    // stray shorter/foreign key can never leak into the wrong shard.
                    if in_shard(key_bytes[1]) {
                        // Hard cap: bail out before the set can exceed the budget; the caller splits
                        // this range and retries the (smaller) halves.
                        if spent_set.len() as u64 >= max_spent_entries {
                            return Ok(None);
                        }
                        spent_set.insert(Box::from(key_bytes));
                    }
                }
                next = match cursor.get(None, None, lmdb_sys::MDB_NEXT) {
                    Ok((key, _value)) => key,
                    Err(lmdb::Error::NotFound) => None,
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                };
            }
        }

        // (2) Sequential pass over block transparent data, height-ascending.
        let mut shard_acc = FinalisedTxOutSetInfoAccumulator::empty();
        let mut height = GENESIS_HEIGHT.0;
        while height <= db_tip.0 {
            let block_height = Height::try_from(height)
                .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
            let height_bytes = block_height.to_bytes()?;

            let transparent_tx_list = {
                let raw = txn
                    .get(self.transparent, &height_bytes)
                    .map_err(FinalisedStateError::LmdbError)?;
                let entry =
                    StoredEntryVar::<TransparentTxList>::from_bytes(raw).map_err(|error| {
                        FinalisedStateError::Custom(format!("transparent corrupt data: {error}"))
                    })?;
                if !entry.verify(&height_bytes) {
                    return Err(FinalisedStateError::Custom(
                        "transparent checksum mismatch".to_string(),
                    ));
                }
                entry.inner().clone()
            };

            let txids = {
                let raw = txn
                    .get(self.txids, &height_bytes)
                    .map_err(FinalisedStateError::LmdbError)?;
                let entry = StoredEntryVar::<TxidList>::from_bytes(raw).map_err(|error| {
                    FinalisedStateError::Custom(format!("txids corrupt data: {error}"))
                })?;
                if !entry.verify(&height_bytes) {
                    return Err(FinalisedStateError::Custom(
                        "txids checksum mismatch".to_string(),
                    ));
                }
                entry.inner().txids().to_vec()
            };

            for (tx_index, tx_opt) in transparent_tx_list.tx().iter().enumerate() {
                let txid = txids.get(tx_index).ok_or_else(|| {
                    FinalisedStateError::Custom(format!(
                        "txid/transparent length mismatch at height {height}"
                    ))
                })?;

                // A tx's outputs are removed by spends keyed under the same txid, so the whole
                // tx belongs to exactly one shard.
                if !in_shard(txid.0[0]) {
                    continue;
                }

                let Some(transparent_tx) = tx_opt else {
                    continue;
                };

                let mut tx_has_unspent = false;
                for (out_index, output) in transparent_tx.outputs().iter().enumerate() {
                    if is_unspendable_tx_out(output) {
                        continue;
                    }

                    let outpoint = Outpoint::new(txid.0, out_index as u32);
                    let outpoint_key = outpoint.to_bytes()?;
                    if spent_set.contains(outpoint_key.as_slice()) {
                        // Created then spent at/below the tip: cancels out of the live set.
                        continue;
                    }

                    shard_acc
                        .apply_added_output(&outpoint, output)
                        .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                    tx_has_unspent = true;
                }

                if tx_has_unspent {
                    shard_acc.transactions =
                        shard_acc.transactions.checked_add(1).ok_or_else(|| {
                            FinalisedStateError::Custom(
                                "txout-set accumulator transactions overflow".to_string(),
                            )
                        })?;
                }
            }

            height += 1;
        }

        Ok(Some(shard_acc))
    }

    /// Reads the height the persisted txout-set accumulator currently reflects, or `None` if it has
    /// never been built (fresh database / pre-migration). Drives the rebuild-vs-incremental dispatch
    /// in [`DbV1::write_blocks_to_height`].
    pub(crate) async fn read_tx_out_set_accumulator_built_height(
        &self,
    ) -> Result<Option<Height>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            match txn.get(self.metadata, &TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY) {
                Ok(bytes) => {
                    let entry = StoredEntryFixed::<Height>::from_bytes(bytes).map_err(|error| {
                        FinalisedStateError::Custom(format!(
                            "accumulator built-height decode error: {error}"
                        ))
                    })?;
                    if !entry.verify(TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY) {
                        return Err(FinalisedStateError::Custom(
                            "accumulator built-height checksum mismatch".to_string(),
                        ));
                    }
                    Ok(Some(*entry.inner()))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(error) => Err(FinalisedStateError::LmdbError(error)),
            }
        })
    }

    /// Advances the persisted txout-set accumulator from `built` to `tip` by applying only the delta
    /// of the just-written blocks `(built, tip]`, then persists the accumulator and its watermark.
    ///
    /// This is the steady-state alternative to [`DbV1::rebuild_tx_out_set_accumulator`]: instead of
    /// re-scanning the whole chain it reads only the range's blocks plus a bounded number of point
    /// lookups, so its cost is O(range) and independent of chain length. The result is identical to a
    /// from-genesis rebuild at `tip`: the stored accumulator already reflects the UTXO set at `built`
    /// (the watermark invariant), and the UTXO set at `tip` differs from it only by outputs created
    /// in the range and still unspent (added), minus outputs that were unspent at `built` and spent
    /// within the range (removed); a create-and-spend within the range cancels.
    ///
    /// The four additive/XOR fields are exactly `created − spent` over the range. `transactions`
    /// (count of txs with ≥1 unspent spendable output) is the only non-additive field; its delta is
    /// computed against the *final* on-disk state — the `spent` table already covers every spend up
    /// to `tip`, so "unspent at the tip" is a direct lookup and no per-block "as of height"
    /// bookkeeping is needed:
    /// - **Set A** — each tx created in the range: `+1` iff it still has a live output at the tip.
    /// - **Set B** — each prior tx (created at/before `built`) we spent a *spendable* output of: it
    ///   was necessarily counted at `built` (that output was live then), so `-1` iff its last live
    ///   output is now gone. The two sets are disjoint by creation height and cover every change.
    ///
    /// WARNING: must be called only from the single DB control task (it does an unsynchronised
    /// read-modify-write of the accumulator singleton), and only when the accumulator has already
    /// been built to `built` (`built < tip`).
    pub(crate) async fn update_tx_out_set_accumulator_for_range(
        &self,
        built: Height,
        tip: Height,
    ) -> Result<(), FinalisedStateError> {
        let accumulator = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            // Load the accumulator the stored watermark refers to (it must exist on this path).
            let mut accumulator = {
                let raw = match txn.get(
                    self.tx_out_set_info_accumulator,
                    &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                ) {
                    Ok(value) => value,
                    Err(lmdb::Error::NotFound) => {
                        return Err(FinalisedStateError::Custom(
                            "txout-set accumulator missing during incremental update".to_string(),
                        ))
                    }
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                };
                let entry = StoredEntryFixed::<FinalisedTxOutSetInfoAccumulator>::from_bytes(raw)
                    .map_err(|error| {
                    FinalisedStateError::Custom(format!(
                        "txout-set accumulator decode error: {error}"
                    ))
                })?;
                if !entry.verify(TX_OUT_SET_INFO_ACCUMULATOR_KEY) {
                    return Err(FinalisedStateError::Custom(
                        "txout-set accumulator checksum mismatch".to_string(),
                    ));
                }
                entry.item
            };

            // ---- Pass 1: scan the range blocks `(built, tip]`. ----
            // Each created spendable output is XORed in immediately; spends are removed in pass 2.
            let mut range_txids: HashSet<[u8; 32]> = HashSet::new();
            // Spendable created outputs keyed by outpoint bytes, to resolve same-range spends with no
            // disk read.
            let mut range_outputs: HashMap<Vec<u8>, TxOutCompact> = HashMap::new();
            // Spendable created outpoints grouped by creating txid, for the Set A recount.
            let mut created_outpoints_by_tx: HashMap<[u8; 32], Vec<Outpoint>> = HashMap::new();
            // Every (non-null) prev-outpoint spent by the range.
            let mut spends: Vec<Outpoint> = Vec::new();

            let mut height = built.0 + 1;
            while height <= tip.0 {
                let height_bytes = Height(height).to_bytes()?;

                let transparent_tx_list = {
                    let raw = txn
                        .get(self.transparent, &height_bytes)
                        .map_err(FinalisedStateError::LmdbError)?;
                    let entry =
                        StoredEntryVar::<TransparentTxList>::from_bytes(raw).map_err(|error| {
                            FinalisedStateError::Custom(format!(
                                "transparent corrupt data: {error}"
                            ))
                        })?;
                    if !entry.verify(&height_bytes) {
                        return Err(FinalisedStateError::Custom(
                            "transparent checksum mismatch".to_string(),
                        ));
                    }
                    entry.inner().clone()
                };

                let txids = {
                    let raw = txn
                        .get(self.txids, &height_bytes)
                        .map_err(FinalisedStateError::LmdbError)?;
                    let entry = StoredEntryVar::<TxidList>::from_bytes(raw).map_err(|error| {
                        FinalisedStateError::Custom(format!("txids corrupt data: {error}"))
                    })?;
                    if !entry.verify(&height_bytes) {
                        return Err(FinalisedStateError::Custom(
                            "txids checksum mismatch".to_string(),
                        ));
                    }
                    entry.inner().txids().to_vec()
                };

                for (tx_index, tx_opt) in transparent_tx_list.tx().iter().enumerate() {
                    let txid = txids.get(tx_index).ok_or_else(|| {
                        FinalisedStateError::Custom(format!(
                            "txid/transparent length mismatch at height {height}"
                        ))
                    })?;
                    range_txids.insert(txid.0);

                    let Some(transparent_tx) = tx_opt else {
                        continue;
                    };

                    for (out_index, output) in transparent_tx.outputs().iter().enumerate() {
                        if is_unspendable_tx_out(output) {
                            continue;
                        }
                        let outpoint = Outpoint::new(txid.0, out_index as u32);
                        accumulator
                            .apply_added_output(&outpoint, output)
                            .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                        range_outputs.insert(outpoint.to_bytes()?, *output);
                        created_outpoints_by_tx
                            .entry(txid.0)
                            .or_default()
                            .push(outpoint);
                    }

                    spends.extend(transparent_tx.spent_outpoints());
                }

                height += 1;
            }

            // ---- Pass 2: remove spent outputs (XOR out) and collect prior spent txids for Set B. ----
            let mut prior_spent_txids: HashSet<[u8; 32]> = HashSet::new();
            for outpoint in &spends {
                let prev_txid = *outpoint.prev_txid();
                let outpoint_bytes = outpoint.to_bytes()?;

                let prev_output = if range_txids.contains(&prev_txid) {
                    // Created within the range: resolve from memory. A miss means the referenced
                    // output was unspendable (never added), so there is nothing to remove.
                    match range_outputs.get(&outpoint_bytes) {
                        Some(output) => *output,
                        None => continue,
                    }
                } else {
                    // Created at/before `built`: resolve from disk. An unspendable prev-output was
                    // never in the set, so it is neither removed nor a Set B trigger.
                    let Some(output) = self.resolve_prev_output_in_txn(&txn, *outpoint)? else {
                        return Err(FinalisedStateError::Custom(format!(
                            "incremental accumulator update: previous output {outpoint:?} not found"
                        )));
                    };
                    if is_unspendable_tx_out(&output) {
                        continue;
                    }
                    prior_spent_txids.insert(prev_txid);
                    output
                };

                accumulator
                    .apply_removed_output(outpoint, &prev_output)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
            }

            // ---- Pass 3: `transactions` delta (the only non-additive field). ----
            // An outpoint is "unspent at the tip" iff it is absent from the `spent` table, which now
            // covers all spends up to `tip`.
            let mut transactions_delta: i64 = 0;

            // Set A: a tx created in the range contributes +1 iff it still has a live output.
            for outpoints in created_outpoints_by_tx.values() {
                let mut has_unspent = false;
                for outpoint in outpoints {
                    if self.is_outpoint_unspent_in_txn(&txn, outpoint)? {
                        has_unspent = true;
                        break;
                    }
                }
                if has_unspent {
                    transactions_delta += 1;
                }
            }

            // Set B: a prior tx we spent a spendable output of contributes -1 iff its last live
            // output is now gone.
            for prev_txid in &prior_spent_txids {
                let Some(prev_tx) =
                    self.get_transparent_tx_in_txn(&txn, &TransactionHash(*prev_txid))?
                else {
                    return Err(FinalisedStateError::Custom(format!(
                        "incremental accumulator update: spent transaction {prev_txid:?} missing"
                    )));
                };
                let mut all_spent = true;
                for (out_index, output) in prev_tx.outputs().iter().enumerate() {
                    if is_unspendable_tx_out(output) {
                        continue;
                    }
                    let outpoint = Outpoint::new(*prev_txid, out_index as u32);
                    if self.is_outpoint_unspent_in_txn(&txn, &outpoint)? {
                        all_spent = false;
                        break;
                    }
                }
                if all_spent {
                    transactions_delta -= 1;
                }
            }

            accumulator.transactions = i64::try_from(accumulator.transactions)
                .ok()
                .and_then(|count| count.checked_add(transactions_delta))
                .and_then(|count| u64::try_from(count).ok())
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator transactions delta under/overflow".to_string(),
                    )
                })?;

            Ok::<_, FinalisedStateError>(accumulator)
        })?;

        // Persist the updated accumulator and advance the watermark to `tip` atomically, then force
        // durability — mirroring `rebuild_tx_out_set_accumulator`.
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            self.put_tx_out_set_accumulator(&mut txn, accumulator)?;
            self.put_tx_out_set_accumulator_watermark(&mut txn, tip)?;

            txn.commit()?;
            self.env.sync(true)?;

            Ok::<_, FinalisedStateError>(())
        })
    }

    /// `true` iff `outpoint` is absent from the `spent` table (read through `txn`).
    fn is_outpoint_unspent_in_txn<T: lmdb::Transaction>(
        &self,
        txn: &T,
        outpoint: &Outpoint,
    ) -> Result<bool, FinalisedStateError> {
        match txn.get(self.spent, &outpoint.to_bytes()?) {
            Ok(_) => Ok(false),
            Err(lmdb::Error::NotFound) => Ok(true),
            Err(error) => Err(FinalisedStateError::LmdbError(error)),
        }
    }

    /// Resolves a txid to its [`TxLocation`] via the `txid_location` index, read through `txn`.
    fn find_txid_location_in_txn<T: lmdb::Transaction>(
        &self,
        txn: &T,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        let key: [u8; 32] = (*txid).into();
        match txn.get(self.txid_location, &key) {
            Ok(bytes) => {
                let entry = StoredEntryFixed::<TxLocation>::from_bytes(bytes).map_err(|error| {
                    FinalisedStateError::Custom(format!("corrupt txid_location entry: {error}"))
                })?;
                if !entry.verify(key) {
                    return Err(FinalisedStateError::Custom(
                        "txid_location entry checksum mismatch".to_string(),
                    ));
                }
                Ok(Some(*entry.inner()))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(error) => Err(FinalisedStateError::LmdbError(error)),
        }
    }

    /// Resolves the previous [`TxOutCompact`] for `outpoint`, read through `txn` (no new txn).
    fn resolve_prev_output_in_txn<T: lmdb::Transaction>(
        &self,
        txn: &T,
        outpoint: Outpoint,
    ) -> Result<Option<TxOutCompact>, FinalisedStateError> {
        let prev_txid = TransactionHash::from(*outpoint.prev_txid());
        let Some(location) = self.find_txid_location_in_txn(txn, &prev_txid)? else {
            return Ok(None);
        };
        let height_bytes = Height(location.block_height()).to_bytes()?;
        let stored = match txn.get(self.transparent, &height_bytes) {
            Ok(bytes) => bytes,
            Err(lmdb::Error::NotFound) => return Ok(None),
            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
        };
        Self::find_txout_in_stored_transparent_tx_list(
            stored,
            location.tx_index() as usize,
            outpoint.prev_index() as usize,
        )
    }

    /// Fetches the full [`TransparentCompactTx`] for `txid`, read through `txn` (no new txn).
    fn get_transparent_tx_in_txn<T: lmdb::Transaction>(
        &self,
        txn: &T,
        txid: &TransactionHash,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        let Some(location) = self.find_txid_location_in_txn(txn, txid)? else {
            return Ok(None);
        };
        let height_bytes = Height(location.block_height()).to_bytes()?;
        let raw = match txn.get(self.transparent, &height_bytes) {
            Ok(bytes) => bytes,
            Err(lmdb::Error::NotFound) => return Ok(None),
            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
        };
        let entry = StoredEntryVar::<TransparentTxList>::from_bytes(raw).map_err(|error| {
            FinalisedStateError::Custom(format!("transparent corrupt data: {error}"))
        })?;
        if !entry.verify(&height_bytes) {
            return Err(FinalisedStateError::Custom(
                "transparent checksum mismatch".to_string(),
            ));
        }
        Ok(entry
            .inner()
            .tx()
            .get(location.tx_index() as usize)
            .cloned()
            .flatten())
    }
}

/// `FinalisedSource` dispatch for the accumulator capability, co-located with the V1
/// implementation it forwards to. V1-only; ephemeral backends have no accumulator.
impl<T: BlockchainSource> FinalisedSource<T> {
    /// Provides access to the finalised txout-set accumulator DB table.
    pub(crate) fn tx_out_set_info_accumulator_db(&self) -> Result<Database, FinalisedStateError> {
        Ok(self
            .require_v1("v1 tx_out_set_info_accumulator db not available")?
            .tx_out_set_info_accumulator_db())
    }

    /// Bulk-rebuilds the finalised txout-set accumulator to the current tip and persists it (V1
    /// only).
    ///
    /// Recomputes the accumulator from the finalised `transparent` + `spent` tables via sequential
    /// scans and writes the singleton plus its freshness watermark. Replaces the per-block
    /// accumulator maintenance that dominated sync time at sandblast height; used by
    /// `sync_to_height` after a catch-up run and by the v1.2 migration's accumulator stage.
    pub(crate) async fn rebuild_tx_out_set_accumulator(&self) -> Result<(), FinalisedStateError> {
        self.require_v1("v1 txout-set accumulator builder")?
            .rebuild_tx_out_set_accumulator()
            .await
    }

    /// Runs the v1.2.0 migration's Stage C: bulk-rebuilds the txout-set accumulator from the
    /// finalised `transparent` + `spent` tables built by Stage B. Idempotent — it never trusts an
    /// existing accumulator, so a stale per-block value from an interrupted prior run is discarded
    /// and replaced. Emits the stage's start / elapsed-on-complete logs; `db_tip` is the height
    /// being built to.
    pub(crate) async fn run_v1_2_migration_accumulator_stage(
        &self,
        db_tip: u32,
    ) -> Result<(), FinalisedStateError> {
        let stage_started = std::time::Instant::now();
        info!(
            db_tip,
            "v1.2.0 migration Stage C: building txout-set accumulator"
        );
        self.rebuild_tx_out_set_accumulator().await?;
        info!(
            db_tip,
            elapsed = ?stage_started.elapsed(),
            "v1.2.0 migration Stage C complete"
        );
        Ok(())
    }
}

/// Test oracle: recomputes the expected accumulator independently from the backend's
/// `transparent` + `spent` tables, for assertions in the v1.1->v1.2 migration tests.
#[cfg(test)]
pub(crate) async fn expected_tx_out_set_info_accumulator(
    database_backend: &FinalisedSource<MockchainSource>,
    max_height: Height,
) -> FinalisedTxOutSetInfoAccumulator {
    let environment = database_backend.env().unwrap();
    let spent_database = database_backend.spent_db().unwrap();

    let mut expected_accumulator = FinalisedTxOutSetInfoAccumulator::empty();

    for height_raw in 0..=max_height.0 {
        let height = Height(height_raw);

        let transparent_transaction_list = database_backend
            .get_block_transparent(height)
            .await
            .unwrap();

        for (transaction_index, transparent_transaction_opt) in
            transparent_transaction_list.tx().iter().enumerate()
        {
            let Some(transparent_transaction) = transparent_transaction_opt else {
                continue;
            };

            if transparent_transaction.outputs().is_empty() {
                continue;
            }

            let transaction_index = u16::try_from(transaction_index).unwrap();
            let transaction_location = TxLocation::new(height.0, transaction_index);

            let transaction_hash = database_backend
                .get_txid(transaction_location)
                .await
                .unwrap();

            let mut unspent_outputs_for_transaction = 0u64;

            let transaction = environment.begin_ro_txn().unwrap();

            for (output_index, output) in transparent_transaction.outputs().iter().enumerate() {
                // The accumulator excludes NonStandard (unspendable) outputs from every field —
                // see `is_unspendable_tx_out`. The migration oracle must skip them too,
                // otherwise it overcounts compared to the on-disk accumulator value the
                // migration backfilled.
                if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                    continue;
                }

                let output_index = u32::try_from(output_index).unwrap();
                let outpoint = Outpoint::new(transaction_hash.0, output_index);
                let outpoint_bytes = outpoint.to_bytes().unwrap();

                let still_unspent = match transaction.get(spent_database, &outpoint_bytes) {
                    Ok(spent_bytes) => {
                        let spent_entry =
                            StoredEntryFixed::<TxLocation>::from_bytes(spent_bytes).unwrap();

                        assert!(
                            spent_entry.verify(&outpoint_bytes),
                            "spent checksum mismatch for outpoint {:?}",
                            outpoint
                        );

                        spent_entry.inner().block_height() > max_height.0
                    }

                    Err(lmdb::Error::NotFound) => true,

                    Err(error) => panic!(
                        "failed to read spent entry for outpoint {:?}: {error}",
                        outpoint
                    ),
                };

                if still_unspent {
                    unspent_outputs_for_transaction += 1;
                    expected_accumulator
                        .apply_added_output(&outpoint, output)
                        .unwrap();
                }
            }

            if unspent_outputs_for_transaction > 0 {
                expected_accumulator.transactions += 1;
            }
        }
    }

    expected_accumulator
}

/// Test assertion: the backend's maintained accumulator equals the independently recomputed
/// [`expected_tx_out_set_info_accumulator`]. Used by the v1.1->v1.2 migration tests.
#[cfg(test)]
pub(crate) async fn assert_tx_out_set_info_accumulator_matches_transparent_data(
    database_backend: &FinalisedSource<MockchainSource>,
) {
    let database_height = database_backend.db_height().await.unwrap().unwrap();

    let expected_accumulator =
        expected_tx_out_set_info_accumulator(database_backend, database_height).await;

    let actual_accumulator = database_backend
        .get_tx_out_set_info_accumulator()
        .await
        .unwrap();

    assert_eq!(
        actual_accumulator, expected_accumulator,
        "txout-set accumulator does not match transparent data and spent index"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::finalised_state::FinalisedState;
    use crate::chain_index::tests::finalised_state::v1::load_vectors_and_spawn_and_sync_v1_zaino_db;
    use crate::chain_index::tests::init_tracing;
    use crate::chain_index::tests::vectors::{
        build_mockchain_source, indexed_block_chain, load_test_vectors, TestVectorBlockData,
        TestVectorData,
    };
    use crate::chain_index::types::db::metadata::{
        FinalisedTxOutSetInfoAccumulator, ZAINO_TXOUTSET_ENTRY_LEN,
    };
    use crate::ChainIndexConfig;
    use std::sync::Arc;
    use tempfile::TempDir;
    use zaino_common::network::ActivationHeights;
    use zaino_common::{DatabaseConfig, StorageConfig, SyncWriteBatchSize};

    fn p2pkh_out(value: u64) -> TxOutCompact {
        TxOutCompact::new(value, [0x11; 20], 0).expect("P2PKH script_type should be valid")
    }

    fn outpoint(txid_byte: u8, index: u32) -> Outpoint {
        Outpoint::new([txid_byte; 32], index)
    }

    #[test]
    fn entries_delta_add_then_remove_roundtrips() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let entries = vec![
            (outpoint(0x01, 0), p2pkh_out(100)),
            (outpoint(0x02, 1), p2pkh_out(200)),
        ];

        apply_tx_out_set_entries_delta(&mut acc, &entries, true).expect("add should succeed");

        assert_eq!(acc.total_zatoshis, 300);
        assert_eq!(acc.bytes_serialized, 2 * ZAINO_TXOUTSET_ENTRY_LEN);

        apply_tx_out_set_entries_delta(&mut acc, &entries, false).expect("remove should succeed");

        assert_eq!(acc, FinalisedTxOutSetInfoAccumulator::empty());
    }

    #[test]
    fn entries_delta_remove_on_empty_returns_underflow_error() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let entries = vec![(outpoint(0xAA, 0), p2pkh_out(500))];

        let err = apply_tx_out_set_entries_delta(&mut acc, &entries, false);

        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("underflow"), "expected underflow, got: {msg}");
    }

    #[test]
    fn entries_delta_ignores_empty_slice() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        acc.total_zatoshis = 999;
        acc.bytes_serialized = 65;
        acc.transaction_outputs = 1;

        let snapshot = acc;
        apply_tx_out_set_entries_delta(&mut acc, &[], true).expect("empty add should succeed");
        assert_eq!(acc, snapshot);

        apply_tx_out_set_entries_delta(&mut acc, &[], false).expect("empty remove should succeed");
        assert_eq!(acc, snapshot);
    }

    #[test]
    fn in_block_transitions_spendable_only() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let tx_hash = TransactionHash([0xAB; 32]);

        let created_counts = HashMap::from([(tx_hash, 3)]);
        let spendable_counts = HashMap::from([(tx_hash, 2)]);
        let spent_indices_by_tx = HashMap::from([(tx_hash, HashSet::from([0]))]);
        let spendable_spent_count_by_tx = HashMap::from([(tx_hash, 1)]);

        apply_in_block_transitions(
            &mut acc,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            1,
            AccumulatorDirection::Apply,
        )
        .expect("apply should succeed");

        assert_eq!(acc.transaction_outputs, 1, "2 created - 1 spent = 1");
        assert_eq!(
            acc.transactions, 1,
            "tx enters UTXO set: 2 spendable > 1 spent"
        );
    }

    #[test]
    fn in_block_transitions_unspendable_spend_does_not_inflate_count() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let tx_hash = TransactionHash([0xCC; 32]);

        // Tx has 2 total outputs, 1 spendable (P2PKH at idx 0) + 1 unspendable (NonStandard at idx 1).
        // The unspendable output is spent in the same block, but after filtering it's excluded.
        let created_counts = HashMap::from([(tx_hash, 2)]);
        let spendable_counts = HashMap::from([(tx_hash, 1)]);
        // Full indices include the unspendable spend for positional check.
        let spent_indices_by_tx = HashMap::from([(tx_hash, HashSet::from([1]))]);
        // After filtering: no spendable outputs were spent.
        let spendable_spent_count_by_tx = HashMap::new();

        apply_in_block_transitions(
            &mut acc,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            0,
            AccumulatorDirection::Apply,
        )
        .expect("apply should succeed");

        assert_eq!(
            acc.transaction_outputs, 1,
            "1 spendable created - 0 spendable spent"
        );
        assert_eq!(
            acc.transactions, 1,
            "tx enters UTXO set: 1 spendable > 0 spent"
        );
    }

    #[test]
    fn in_block_transitions_all_spendable_spent_same_block() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let tx_hash = TransactionHash([0xDD; 32]);

        let created_counts = HashMap::from([(tx_hash, 2)]);
        let spendable_counts = HashMap::from([(tx_hash, 2)]);
        let spent_indices_by_tx = HashMap::from([(tx_hash, HashSet::from([0, 1]))]);
        let spendable_spent_count_by_tx = HashMap::from([(tx_hash, 2)]);

        apply_in_block_transitions(
            &mut acc,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            2,
            AccumulatorDirection::Apply,
        )
        .expect("apply should succeed");

        assert_eq!(acc.transaction_outputs, 0, "2 created - 2 spent = 0");
        assert_eq!(acc.transactions, 0, "tx never enters UTXO set: 2 == 2");
    }

    #[test]
    fn in_block_transitions_reverse_direction() {
        let tx_hash = TransactionHash([0xEE; 32]);

        let created_counts = HashMap::from([(tx_hash, 2)]);
        let spendable_counts = HashMap::from([(tx_hash, 2)]);
        let spent_indices_by_tx = HashMap::new();
        let spendable_spent_count_by_tx = HashMap::new();

        // Simulate state after writing a block that created 2 spendable outputs.
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        apply_in_block_transitions(
            &mut acc,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            0,
            AccumulatorDirection::Apply,
        )
        .expect("forward apply should succeed");

        assert_eq!(acc.transaction_outputs, 2);
        assert_eq!(acc.transactions, 1);

        // Reverse should return to empty.
        apply_in_block_transitions(
            &mut acc,
            &created_counts,
            &spendable_counts,
            &spent_indices_by_tx,
            &spendable_spent_count_by_tx,
            0,
            AccumulatorDirection::Reverse,
        )
        .expect("reverse should succeed");

        assert_eq!(acc, FinalisedTxOutSetInfoAccumulator::empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tx_out_set_info_accumulator_updates_on_write() {
        init_tracing();

        // Load the regtest vectors, write every vector block into FinalisedState, and wait until the
        // finalised state has finished its startup/background validation.
        let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
            load_vectors_and_spawn_and_sync_v1_zaino_db().await;

        zaino_db.wait_until_ready().await;

        let db_reader = Arc::new(zaino_db).to_reader();

        // Build the expected UTXO set directly from the same vector blocks.
        //
        // Map shape:
        //   txid -> { output_index -> TxOutCompact }
        //
        // From this we derive every accumulator field:
        //   transactions         = number of txids with at least one unspent output
        //   transaction_outputs  = total number of unspent transparent outputs
        //   bytes_serialized     = transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN
        //   hash_serialized      = XOR of tx_out_set_entry_digest over all unspent outputs
        //   total_zatoshis       = sum of `value` over all unspent outputs
        let mut unspent_output_indices_by_transaction_hash: HashMap<
            TransactionHash,
            HashMap<u32, crate::TxOutCompact>,
        > = HashMap::new();

        for chain_block in indexed_block_chain(&blocks) {
            for transaction in chain_block.transactions() {
                // First apply spends, removing spent transparent outputs from the expected UTXO set.
                for outpoint in transaction.transparent().spent_outpoints() {
                    let previous_transaction_hash = TransactionHash::from(*outpoint.prev_txid());

                    let unspent_output_indices = unspent_output_indices_by_transaction_hash
                        .get_mut(&previous_transaction_hash)
                        .unwrap_or_else(|| {
                            panic!(
                            "test vectors spend unknown transaction {previous_transaction_hash:?}"
                        )
                        });

                    assert!(
                        unspent_output_indices
                            .remove(&outpoint.prev_index())
                            .is_some(),
                        "test vectors spend unknown output: transaction {:?}, output {}",
                        previous_transaction_hash,
                        outpoint.prev_index()
                    );

                    // If a transaction has no remaining unspent outputs, it should no longer
                    // contribute to the accumulator's `transactions` count.
                    if unspent_output_indices.is_empty() {
                        unspent_output_indices_by_transaction_hash
                            .remove(&previous_transaction_hash);
                    }
                }

                // Then apply outputs, adding newly-created transparent outputs to the expected UTXO set.
                if transaction.transparent().outputs().is_empty() {
                    continue;
                }

                let transaction_hash = *transaction.txid();

                let unspent_output_indices = unspent_output_indices_by_transaction_hash
                    .entry(transaction_hash)
                    .or_default();

                for (output_index, output) in transaction.transparent().outputs().iter().enumerate()
                {
                    // The accumulator skips NonStandard (unspendable) outputs — see
                    // `is_unspendable_tx_out` in
                    // `chain_index::types::db::metadata`. The oracle must mirror that.
                    if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                        continue;
                    }

                    let output_index = u32::try_from(output_index).unwrap();

                    assert!(
                    unspent_output_indices
                        .insert(output_index, *output)
                        .is_none(),
                    "test vectors duplicate output index: transaction {transaction_hash:?}, output {output_index}"
                );
                }

                // If the transaction had only NonStandard outputs, drop the empty entry so it
                // doesn't inflate the expected `transactions` count.
                if unspent_output_indices.is_empty() {
                    unspent_output_indices_by_transaction_hash.remove(&transaction_hash);
                }
            }
        }

        let expected_accumulator =
            accumulator_from_unspent_map(&unspent_output_indices_by_transaction_hash);

        // Check that the accumulator maintained by write_block matches the independently
        // reconstructed expected UTXO-set counts.
        let actual_accumulator = db_reader.get_tx_out_set_info_accumulator().await.unwrap();

        assert_eq!(expected_accumulator, actual_accumulator);
    }

    /// The bulk sequential accumulator builder must produce exactly the accumulator that the
    /// per-block incremental write path maintained, for every shard count. Sharding partitions the
    /// work by creating-txid prefix and recombines the partials; the result must be shard-count
    /// independent.
    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_tx_out_set_accumulator_builder_matches_incremental() {
        init_tracing();

        let (_data, _db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;
        zaino_db.wait_until_ready().await;

        use crate::chain_index::finalised_state::capability::{
            CapabilityRequest, DbRead, TransparentHistExt,
        };

        let backend = zaino_db
            .backend_for_cap(CapabilityRequest::WriteCore)
            .unwrap();

        let db_tip = backend.db_height().await.unwrap().unwrap();
        let incremental = backend.get_tx_out_set_info_accumulator().await.unwrap();

        // 1 = single optimal pass; >1 exercises the sharded multi-pass recombination; 256 = one
        // first-byte value per shard (maximal sharding).
        for shards in [1u16, 2, 4, 256] {
            // Cap = u64::MAX: the initial partition is used verbatim (no bisection).
            let built = tokio::task::block_in_place(|| {
                backend.build_tx_out_set_accumulator_blocking(db_tip, shards, u64::MAX)
            })
            .unwrap();

            assert_eq!(
                built, incremental,
                "bulk builder (shards={shards}) must equal the incrementally-maintained accumulator"
            );

            // Tiny per-shard caps force the strict memory bound to bisect the initial ranges to
            // varying depths. Whenever the build succeeds it must still equal the incremental
            // accumulator (the bisected ranges remain a disjoint cover of the first-byte space, and
            // recombination is order-independent); if a single first-byte bucket genuinely exceeds
            // the cap the builder fails fast by design (the strict bound), which is also acceptable.
            for max_spent_entries in [16u64, 8, 4, 2, 1] {
                if let Ok(built_capped) = tokio::task::block_in_place(|| {
                    backend.build_tx_out_set_accumulator_blocking(db_tip, shards, max_spent_entries)
                }) {
                    assert_eq!(
                        built_capped, incremental,
                        "strict-capped bulk builder (shards={shards}, cap={max_spent_entries}) must \
                         equal the incremental accumulator"
                    );
                }
            }
        }
    }

    /// The accumulator rebuild auto-shards to keep the per-shard in-memory spent set within the
    /// configured memory budget: a generous budget yields a single optimal pass, while a budget
    /// smaller than the spent set forces multiple shards (capped at 256). This is the OOM guard for
    /// memory-constrained hosts.
    #[tokio::test(flavor = "multi_thread")]
    async fn accumulator_build_shards_scale_to_memory_budget() {
        init_tracing();

        let (_data, _db_dir, zaino_db) = load_vectors_and_spawn_and_sync_v1_zaino_db().await;
        zaino_db.wait_until_ready().await;

        let backend = zaino_db
            .backend_for_cap(
                crate::chain_index::finalised_state::capability::CapabilityRequest::WriteCore,
            )
            .unwrap();

        // A budget far larger than the (tiny regtest) spent set => the whole set fits in one shard.
        assert_eq!(
            backend.accumulator_build_shards(u64::MAX).unwrap(),
            1,
            "a budget exceeding the spent set must use a single pass"
        );

        // A 1-byte budget => the spent set far exceeds it => more than one shard, never above the cap.
        let constrained = backend.accumulator_build_shards(1).unwrap();
        assert!(
            constrained > 1 && constrained <= 256,
            "a 1-byte budget must force multiple shards (capped at 256), got {constrained}"
        );
    }

    /// Syncs the vector chain to height 200 with the given bulk-write batch budget and returns the
    /// resulting `(db tip, validated tip, txout-set accumulator)`.
    async fn sync_with_batch_budget(
        blocks: Vec<TestVectorBlockData>,
        sync_write_batch_size: SyncWriteBatchSize,
    ) -> (Height, u32, FinalisedTxOutSetInfoAccumulator) {
        use crate::chain_index::finalised_state::capability::{
            CapabilityRequest, DbRead, TransparentHistExt,
        };

        let source = build_mockchain_source(blocks);
        let temp_dir: TempDir = tempfile::tempdir().unwrap();
        let config = ChainIndexConfig {
            storage: StorageConfig {
                database: DatabaseConfig {
                    path: temp_dir.path().to_path_buf(),
                    sync_write_batch_size,
                    ..Default::default()
                },
                ..Default::default()
            },
            ephemeral: false,
            db_version: 1,
            network: ActivationHeights::default().to_regtest_network(),
        };

        let zaino_db = FinalisedState::spawn(config, source.clone()).await.unwrap();
        zaino_db.sync_to_height(Height(200), &source).await.unwrap();
        // Catch-up of >LONG_RUNNING_SYNC_THRESHOLD blocks runs in the background; wait for the
        // persistent DB to actually reach the tip before reading it back.
        zaino_db.wait_until_synced().await;

        let backend = zaino_db
            .backend_for_cap(CapabilityRequest::WriteCore)
            .unwrap();
        let db_tip = backend.db_height().await.unwrap().unwrap();
        let validated_tip = backend.validated_tip_height();
        let accumulator = backend.get_tx_out_set_info_accumulator().await.unwrap();

        (db_tip, validated_tip, accumulator)
    }

    /// The bulk-sync result must be independent of the write-batch budget: a single huge batch and a
    /// one-block-per-batch sync of the same chain must produce an identical db tip, validated tip, and
    /// txout-set accumulator. This exercises the cross-batch continuity chaining, per-batch
    /// `validated_tip` advance, and sorted-insert flush boundaries that a single-batch sync does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn batched_sync_is_batch_size_independent() {
        init_tracing();

        let blocks = load_test_vectors().unwrap().blocks;

        // 1 GiB ≫ the tiny regtest test chain => the whole sync is one batch; 0 GiB => the `.max(1)`
        // floor makes the effective budget 1 byte, so every block exceeds it => one block per batch (a
        // flush + commit + fsync after each block).
        let single_batch = sync_with_batch_budget(blocks.clone(), SyncWriteBatchSize(1)).await;
        let per_block_batches = sync_with_batch_budget(blocks, SyncWriteBatchSize(0)).await;

        assert_eq!(single_batch.0, per_block_batches.0, "db tip must match");
        assert_eq!(
            single_batch.1, per_block_batches.1,
            "validated tip must match"
        );
        assert_eq!(
            single_batch.2, per_block_batches.2,
            "txout-set accumulator must be independent of the write-batch budget"
        );
    }

    /// The incremental range-update path — taken when a catch-up advances an already-built accumulator
    /// by a small range (`write_blocks_to_height`'s steady-state branch) — must produce exactly the
    /// accumulator a full from-genesis rebuild produces at the same tip, for all five fields. This is
    /// the correctness gate for `update_tx_out_set_accumulator_for_range`: with regtest coinbase
    /// maturity `COINBASE_MATURITY`, splitting the sync at that height guarantees the second segment spends outputs
    /// created in the first (exercising the `transactions` "Set B" decrement) as well as outputs both
    /// created and spent within the range (the XOR-cancel case).
    #[tokio::test(flavor = "multi_thread")]
    async fn incremental_accumulator_update_matches_full_rebuild() {
        init_tracing();

        use crate::chain_index::finalised_state::capability::{
            CapabilityRequest, DbRead, TransparentHistExt,
        };
        use zaino_common::consensus::COINBASE_MATURITY;

        let blocks = load_test_vectors().unwrap().blocks;
        let source = build_mockchain_source(blocks);
        let temp_dir: TempDir = tempfile::tempdir().unwrap();
        let config = ChainIndexConfig {
            storage: StorageConfig {
                database: DatabaseConfig {
                    path: temp_dir.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ephemeral: false,
            db_version: 1,
            network: ActivationHeights::default().to_regtest_network(),
        };

        let zaino_db = FinalisedState::spawn(config, source.clone()).await.unwrap();

        // First segment builds the accumulator to `COINBASE_MATURITY` (no watermark yet => full
        // rebuild), the second advances it to the fixture tip => the incremental update path under test.
        zaino_db
            .sync_to_height(Height(COINBASE_MATURITY), &source)
            .await
            .unwrap();
        // Background catch-up (>LONG_RUNNING_SYNC_THRESHOLD); wait for the persistent build + watermark.
        zaino_db.wait_until_synced().await;

        let backend = zaino_db
            .backend_for_cap(CapabilityRequest::WriteCore)
            .unwrap();

        // The watermark must sit at `COINBASE_MATURITY` here: that (together with the gap to the tip
        // <= the incremental cap) pins the next sync to the incremental branch rather than a silent
        // rebuild fallback that would make the comparison below trivial.
        assert_eq!(
            backend
                .read_tx_out_set_accumulator_built_height()
                .await
                .unwrap(),
            Some(Height(COINBASE_MATURITY)),
            "first segment must leave the accumulator watermark at the synced tip"
        );

        zaino_db.sync_to_height(Height(200), &source).await.unwrap();
        // Background catch-up; wait for the incremental accumulator update to advance the watermark.
        zaino_db.wait_until_synced().await;

        let db_tip = backend.db_height().await.unwrap().unwrap();
        assert_eq!(db_tip, Height(200), "both segments must have been synced");
        assert_eq!(
            backend
                .read_tx_out_set_accumulator_built_height()
                .await
                .unwrap(),
            Some(Height(200)),
            "incremental update must advance the watermark to the new tip"
        );

        let incremental = backend.get_tx_out_set_info_accumulator().await.unwrap();
        let from_genesis = tokio::task::block_in_place(|| {
            backend.build_tx_out_set_accumulator_blocking(db_tip, 1, u64::MAX)
        })
        .unwrap();

        assert_eq!(
            incremental, from_genesis,
            "incremental range-update accumulator must equal the from-genesis rebuild at the tip"
        );
    }

    /// Computes the canonical [`FinalisedTxOutSetInfoAccumulator`] for a fully-resolved UTXO set,
    /// used as the source of truth by the write/delete accumulator tests.
    fn accumulator_from_unspent_map(
        unspent: &HashMap<TransactionHash, HashMap<u32, crate::TxOutCompact>>,
    ) -> FinalisedTxOutSetInfoAccumulator {
        use crate::chain_index::types::db::metadata::{
            tx_out_set_entry_digest, ZAINO_TXOUTSET_ENTRY_LEN,
        };
        use crate::Outpoint;

        let mut transaction_outputs = 0u64;
        let mut total_zatoshis = 0u64;
        let mut hash_serialized = [0u8; 32];

        for (txid, outputs) in unspent {
            for (output_index, out) in outputs {
                let outpoint = Outpoint::new(txid.0, *output_index);
                let digest = tx_out_set_entry_digest(&outpoint, out);
                for (dst, src) in hash_serialized.iter_mut().zip(digest.iter()) {
                    *dst ^= *src;
                }
                transaction_outputs += 1;
                total_zatoshis += out.value();
            }
        }

        FinalisedTxOutSetInfoAccumulator {
            transactions: unspent.len() as u64,
            transaction_outputs,
            bytes_serialized: transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN,
            hash_serialized,
            total_zatoshis,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tx_out_set_info_accumulator_updates_on_delete() {
        init_tracing();

        // Load and write all vector blocks, then delete the current tip block.
        // The accumulator should end up matching the UTXO set for all blocks except the deleted tip.
        let (TestVectorData { blocks, .. }, _db_dir, zaino_db) =
            load_vectors_and_spawn_and_sync_v1_zaino_db().await;

        zaino_db.wait_until_ready().await;

        let deleted_block_height = Height(blocks.last().unwrap().height);

        zaino_db
            .delete_block_at_height(deleted_block_height)
            .await
            .unwrap();

        zaino_db.wait_until_ready().await;

        let db_reader = Arc::new(zaino_db).to_reader();

        // Rebuild the expected UTXO set from the vector chain with the deleted tip excluded.
        //
        // This verifies that delete_block reverses every accumulator field:
        //   transactions, transaction_outputs, bytes_serialized, hash_serialized, total_zatoshis.
        let mut unspent_output_indices_by_transaction_hash: HashMap<
            TransactionHash,
            HashMap<u32, crate::TxOutCompact>,
        > = HashMap::new();

        for chain_block in indexed_block_chain(&blocks[..blocks.len() - 1]) {
            for transaction in chain_block.transactions() {
                // Remove any transparent outputs spent by this transaction.
                for outpoint in transaction.transparent().spent_outpoints() {
                    let previous_transaction_hash = TransactionHash::from(*outpoint.prev_txid());

                    let unspent_output_indices = unspent_output_indices_by_transaction_hash
                        .get_mut(&previous_transaction_hash)
                        .unwrap_or_else(|| {
                            panic!(
                            "test vectors spend unknown transaction {previous_transaction_hash:?}"
                        )
                        });

                    assert!(
                        unspent_output_indices
                            .remove(&outpoint.prev_index())
                            .is_some(),
                        "test vectors spend unknown output: transaction {:?}, output {}",
                        previous_transaction_hash,
                        outpoint.prev_index()
                    );

                    if unspent_output_indices.is_empty() {
                        unspent_output_indices_by_transaction_hash
                            .remove(&previous_transaction_hash);
                    }
                }

                // Add this transaction's newly-created transparent outputs.
                if transaction.transparent().outputs().is_empty() {
                    continue;
                }

                let transaction_hash = *transaction.txid();

                let unspent_output_indices = unspent_output_indices_by_transaction_hash
                    .entry(transaction_hash)
                    .or_default();

                for (output_index, output) in transaction.transparent().outputs().iter().enumerate()
                {
                    // The accumulator skips NonStandard (unspendable) outputs — see
                    // `is_unspendable_tx_out` in
                    // `chain_index::types::db::metadata`. The oracle must mirror that.
                    if crate::chain_index::types::db::metadata::is_unspendable_tx_out(output) {
                        continue;
                    }

                    let output_index = u32::try_from(output_index).unwrap();

                    assert!(
                    unspent_output_indices
                        .insert(output_index, *output)
                        .is_none(),
                    "test vectors duplicate output index: transaction {transaction_hash:?}, output {output_index}"
                );
                }

                // If the transaction had only NonStandard outputs, drop the empty entry so it
                // doesn't inflate the expected `transactions` count.
                if unspent_output_indices.is_empty() {
                    unspent_output_indices_by_transaction_hash.remove(&transaction_hash);
                }
            }
        }

        let expected_accumulator =
            accumulator_from_unspent_map(&unspent_output_indices_by_transaction_hash);

        // Check the accumulator persisted by delete_block_at_height/delete_block.
        let actual_accumulator = db_reader.get_tx_out_set_info_accumulator().await.unwrap();

        assert_eq!(expected_accumulator, actual_accumulator);
    }
}
