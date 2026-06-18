//! FinalisedState::V1 transparent address history indexing functionality.

use crate::chain_index::finalised_state::finalised_source::v1::{
    ACCUMULATOR_BUILD_SHARDS, TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
    TX_OUT_SET_INFO_ACCUMULATOR_KEY,
};
use crate::chain_index::types::db::metadata::{
    is_unspendable_tx_out, tx_out_set_entry_digest, FinalisedTxOutSetInfoAccumulator,
    ZAINO_TXOUTSET_ENTRY_LEN,
};

use super::*;

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

/// [`TransparentHistExt`] capability implementation for [`DbV1`].
///
/// Provides address history queries built over the LMDB `DUP_SORT`/`DUP_FIXED` address-history
/// database.
#[async_trait]
impl TransparentHistExt for DbV1 {
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.addr_records(addr_script).await
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.addr_and_index_records(addr_script, tx_location).await
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        self.addr_tx_locations_by_range(addr_script, start_height, end_height)
            .await
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        self.addr_utxos_by_range(addr_script, start_height, end_height)
            .await
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, FinalisedStateError> {
        self.addr_balance_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.get_outpoint_spender(outpoint).await
    }

    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        self.get_outpoint_spenders(outpoints).await
    }

    async fn get_tx_out_set_info_accumulator(
        &self,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        self.get_tx_out_set_info_accumulator().await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut raw_records = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN {
                    continue;
                }
                if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    continue;
                }
                raw_records.push(val.to_vec());
            }

            if raw_records.is_empty() {
                return Ok(None);
            }

            let mut records = Vec::with_capacity(raw_records.len());
            for val in raw_records {
                let entry = StoredEntryFixed::<AddrEventBytes>::from_bytes(&val).map_err(|e| {
                    FinalisedStateError::Custom(format!("addrhist decode error: {e}"))
                })?;
                records.push(entry.item);
            }

            Ok(Some(records))
        })
    }

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        let rec_results = tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let fetch_records_result =
                self.addr_hist_records_by_addr_and_index_in_txn(&ro, &addr_bytes, tx_location);
            ro.commit()?;
            fetch_records_result
        });

        let raw_records = match rec_results {
            Ok(records) => records,
            Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => return Ok(None),
            Err(e) => return Err(e),
        };

        if raw_records.is_empty() {
            return Ok(None);
        }

        let mut records = Vec::with_capacity(raw_records.len());

        for val in raw_records {
            let entry = StoredEntryFixed::<AddrEventBytes>::from_bytes(&val)
                .map_err(|e| FinalisedStateError::Custom(format!("addrhist decode error: {e}")))?;
            records.push(entry.item);
        }

        Ok(Some(records))
    }

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut set: HashSet<TxLocation> = HashSet::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                let tx_index = u16::from_be_bytes([val[6], val[7]]);
                set.insert(TxLocation::new(block_height, tx_index));
            }
            let mut indices: Vec<_> = set.into_iter().collect();
            indices.sort_by_key(|txi| (txi.block_height(), txi.tx_index()));

            if indices.is_empty() {
                Ok(None)
            } else {
                Ok(Some(indices))
            }
        })
    }

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut utxos = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                let flags = val[10];
                if (flags & AddrEventBytes::FLAG_MINED == 0)
                    || (flags & AddrEventBytes::FLAG_SPENT != 0)
                {
                    continue;
                }

                let tx_index = u16::from_be_bytes([val[6], val[7]]);
                let vout = u16::from_be_bytes([val[8], val[9]]);
                let value = u64::from_le_bytes([
                    val[11], val[12], val[13], val[14], val[15], val[16], val[17], val[18],
                ]);

                utxos.push((TxLocation::new(block_height, tx_index), vout, value));
            }

            if utxos.is_empty() {
                Ok(None)
            } else {
                Ok(Some(utxos))
            }
        })
    }

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut balance: i64 = 0;

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if height < start_height.0 || height > end_height.0 {
                    continue;
                }

                let flags = val[10];
                let value = u64::from_le_bytes([
                    val[11], val[12], val[13], val[14], val[15], val[16], val[17], val[18],
                ]) as i64;

                if flags & AddrEventBytes::FLAG_IS_INPUT != 0 {
                    balance -= value;
                } else if flags & AddrEventBytes::FLAG_MINED != 0 {
                    balance += value;
                }
            }

            Ok(balance)
        })
    }

    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        let key = outpoint.to_bytes()?;
        let txn = self.env.begin_ro_txn()?;

        tokio::task::block_in_place(|| match txn.get(self.spent, &key) {
            Ok(bytes) => {
                let entry = StoredEntryFixed::<TxLocation>::from_bytes(bytes).map_err(|e| {
                    FinalisedStateError::Custom(format!("spent entry decode error: {e}"))
                })?;
                Ok(Some(entry.item))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(FinalisedStateError::LmdbError(e)),
        })
    }

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            outpoints
                .into_iter()
                .map(|outpoint| {
                    let key = outpoint.to_bytes()?;
                    match txn.get(self.spent, &key) {
                        Ok(bytes) => {
                            let entry =
                                StoredEntryFixed::<TxLocation>::from_bytes(bytes).map_err(|e| {
                                    FinalisedStateError::Custom(format!(
                                        "spent entry decode error for {outpoint:?}: {e}"
                                    ))
                                })?;
                            Ok(Some(entry.item))
                        }
                        Err(lmdb::Error::NotFound) => Ok(None),
                        Err(e) => Err(FinalisedStateError::LmdbError(e)),
                    }
                })
                .collect()
        })
    }

    /// Returns the finalised-state txout-set accumulator.
    ///
    /// This reads the singleton accumulator entry. It does not compute or repair the accumulator;
    /// accumulator creation, backfill, and updates are handled by migrations and write paths.
    async fn get_tx_out_set_info_accumulator(
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

    // *** Internal DB methods ***

    /// Returns all raw AddrHist records for a given AddrScript and TxLocation.
    ///
    /// Returns a Vec of serialized entries, for given addr_script and ix_index.
    ///
    /// Efficiently filters by matching block + tx index bytes in-place.
    ///
    /// WARNING: This operates *inside* an existing RO txn.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(super) fn addr_hist_records_by_addr_and_index_in_txn(
        &self,
        txn: &lmdb::RoTransaction<'_>,
        addr_script_bytes: &[u8],
        tx_location: TxLocation,
    ) -> Result<Vec<Vec<u8>>, FinalisedStateError> {
        // Open a single cursor.
        let cursor = txn.open_ro_cursor(self.address_history)?;
        let mut results: Vec<Vec<u8>> = Vec::new();

        // Build the seek data prefix that matches the stored bytes:
        // [StoredEntry version, record version, height_be(4), tx_index_be(2)]
        let stored_entry_tag = StoredEntryFixed::<AddrEventBytes>::VERSION;
        let record_tag = AddrEventBytes::VERSION;

        // Reserve the exact number of bytes we need for the SET_RANGE value prefix:
        //
        //  - 1 byte: outer StoredEntry version (StoredEntryFixed::<AddrEventBytes>::VERSION)
        //  - 1 byte: inner record version (AddrEventBytes::VERSION)
        //  - 4 bytes: block_height  (big-endian)
        //  - 2 bytes: tx_index     (big-endian)
        //
        // This minimal prefix (2 + 4 + 2 = 8 bytes) is all we need for MDB_SET_RANGE to
        // position at the first duplicate whose value >= (height, tx_index). Using
        // `with_capacity` avoids reallocations while we build the prefix.  We do *not*
        // append vout/flags/value/checksum here because we only need the leading bytes
        // to seek into the dup-sorted data.
        let mut seek_data = Vec::with_capacity(2 + 4 + 2);
        seek_data.push(stored_entry_tag);
        seek_data.push(record_tag);
        seek_data.extend_from_slice(&tx_location.block_height().to_be_bytes());
        seek_data.extend_from_slice(&tx_location.tx_index().to_be_bytes());

        // Use MDB_SET_RANGE to position the cursor at the first duplicate for this key whose
        // duplicate value is >= seek_data (this is the efficient B-tree seek).
        let op_set_range = lmdb_sys::MDB_SET_RANGE;
        match cursor.get(Some(addr_script_bytes), Some(&seek_data[..]), op_set_range) {
            Ok((maybe_key, mut cur_val)) => {
                // If there's no key, nothing to do
                let mut cur_key = match maybe_key {
                    Some(k) => k,
                    None => return Ok(results),
                };

                // If the seek landed on a different key, there are no candidates for this addr.
                if cur_key.len() != AddrScript::VERSIONED_LEN
                    || &cur_key[..AddrScript::VERSIONED_LEN] != addr_script_bytes
                {
                    return Ok(results);
                }

                // Iterate from the positioned duplicate forward using MDB_NEXT_DUP.
                let op_next_dup = lmdb_sys::MDB_NEXT_DUP;

                loop {
                    // Validate lengths, same as original function.
                    if cur_key.len() != AddrScript::VERSIONED_LEN {
                        return Err(FinalisedStateError::Custom(
                            "address history key length mismatch".into(),
                        ));
                    }
                    if cur_val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                        return Err(FinalisedStateError::Custom(
                            "address history value length mismatch".into(),
                        ));
                    }
                    if cur_val[0] != StoredEntryFixed::<AddrEventBytes>::VERSION
                        || cur_val[1] != AddrEventBytes::VERSION
                    {
                        return Err(FinalisedStateError::Custom(
                            "address history value version tag mismatch".into(),
                        ));
                    }

                    // Read height and tx_index *in-place* from the value bytes:
                    // - [0] stored entry tag
                    // - [1] record tag
                    // - [2..=5] height (BE)
                    // - [6..=7] tx_index (BE)
                    let block_index =
                        u32::from_be_bytes([cur_val[2], cur_val[3], cur_val[4], cur_val[5]]);
                    let tx_idx = u16::from_be_bytes([cur_val[6], cur_val[7]]);

                    if block_index == tx_location.block_height() && tx_idx == tx_location.tx_index()
                    {
                        // Matching entry — collect the full stored entry bytes (same behaviour).
                        results.push(cur_val.to_vec());
                    } else if block_index > tx_location.block_height()
                        || (block_index == tx_location.block_height()
                            && tx_idx > tx_location.tx_index())
                    {
                        // We've passed the requested tx_location in duplicate ordering -> stop
                        // (duplicates are ordered by value, so once we pass, no matches remain).
                        break;
                    }

                    // Advance to the next duplicate for the same key.
                    match cursor.get(None, None, op_next_dup) {
                        Ok((maybe_k, next_val)) => {
                            // If key changed or no key returned, stop.
                            let k = match maybe_k {
                                Some(k) => k,
                                None => break,
                            };
                            if k.len() != AddrScript::VERSIONED_LEN
                                || &k[..AddrScript::VERSIONED_LEN] != addr_script_bytes
                            {
                                break;
                            }
                            // Update cur_key and cur_val and continue.
                            cur_key = k;
                            cur_val = next_val;
                            continue;
                        }
                        Err(lmdb::Error::NotFound) => break,
                        Err(e) => return Err(e.into()),
                    }
                } // loop
            }
            Err(lmdb::Error::NotFound) => {
                // Nothing at or after seek -> empty result
            }
            Err(e) => return Err(e.into()),
        }

        Ok(results)
    }

    /// Inserts a mined-output record into the address‐history map.
    #[cfg(feature = "transparent_address_history_experimental")]
    #[inline]
    pub(super) fn build_transaction_output_histories<'a>(
        map: &mut HashMap<AddrScript, Vec<AddrHistRecord>>,
        tx_location: TxLocation,
        outputs: impl Iterator<Item = (usize, &'a TxOutCompact)>,
    ) {
        for (output_idx, output) in outputs {
            let addr_script = AddrScript::new(*output.script_hash(), output.script_type());
            let output_record = AddrHistRecord::new(
                tx_location,
                output_idx as u16,
                output.value(),
                AddrHistRecord::FLAG_MINED,
            );
            map.entry(addr_script)
                .and_modify(|v| v.push(output_record))
                .or_insert_with(|| vec![output_record]);
        }
    }

    /// Inserts both the “spend” record and the “mined” previous‐output record
    /// (used to update the output record spent in this transaction).
    #[cfg(feature = "transparent_address_history_experimental")]
    #[inline]
    #[allow(clippy::type_complexity)]
    pub(super) fn build_input_history(
        map: &mut HashMap<AddrScript, Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>>,
        input_tx_location: TxLocation,
        input_index: u16,
        input: &TxInCompact,
        prev_output: &TxOutCompact,
        prev_output_tx_location: TxLocation,
    ) {
        let addr_script = AddrScript::new(*prev_output.script_hash(), prev_output.script_type());
        let input_record = AddrHistRecord::new(
            input_tx_location,
            input_index,
            prev_output.value(),
            AddrHistRecord::FLAG_IS_INPUT,
        );
        let prev_output_record = (
            AddrScript::new(*prev_output.script_hash(), prev_output.script_type()),
            AddrHistRecord::new(
                prev_output_tx_location,
                input.prevout_index() as u16,
                prev_output.value(),
                AddrHistRecord::FLAG_MINED,
            ),
        );
        map.entry(addr_script)
            .and_modify(|v| v.push((input_record, prev_output_record)))
            .or_insert_with(|| vec![(input_record, prev_output_record)]);
    }

    /// Delete all `addrhist` duplicates for `addr_bytes` that
    ///   * belong to `block_height`, **and**
    ///   * match the requested record type(s).
    ///
    /// * `delete_inputs`  – remove records whose flag-byte contains FLAG_IS_INPUT
    /// * `delete_outputs` – remove records whose flag-byte contains FLAG_MINED
    ///
    /// `expected` is the number of records to delete;
    ///
    /// WARNING: This operates *inside* an existing RW txn and must **not** commit it.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(super) fn delete_addrhist_dups_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        addr_bytes: &[u8],
        block_height: Height,
        delete_inputs: bool,
        delete_outputs: bool,
        expected: usize,
    ) -> Result<(), FinalisedStateError> {
        if !delete_inputs && !delete_outputs {
            return Err(FinalisedStateError::Custom(
                "called delete_addrhist_dups with neither inputs nor outputs to delete".into(),
            ));
        }
        if expected == 0 {
            return Err(FinalisedStateError::Custom(
                "called delete_addrhist_dups with 0 expected deletes".into(),
            ));
        }

        let mut remaining = expected;
        let height_be = block_height.0.to_be_bytes();

        let mut cur = txn.open_rw_cursor(self.address_history)?;

        match cur
            .get(Some(addr_bytes), None, lmdb_sys::MDB_SET_KEY)
            .and_then(|_| cur.get(None, None, lmdb_sys::MDB_LAST_DUP))
        {
            Ok((_k, mut val)) => loop {
                // Parse AddrEventBytes:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum
                if val.len() == StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                    && val[2..6] == height_be
                {
                    let flags = val[10];
                    let is_input = flags & AddrEventBytes::FLAG_IS_INPUT != 0;
                    let is_output = flags & AddrEventBytes::FLAG_MINED != 0;

                    if (delete_inputs && is_input) || (delete_outputs && is_output) {
                        cur.del(WriteFlags::empty())?;
                        remaining -= 1;
                        if remaining == 0 {
                            break;
                        }
                    }
                } else if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    tracing::warn!("bad addrhist dup (len={})", val.len());
                }

                // step backwards through duplicates
                match cur.get(None, None, lmdb_sys::MDB_PREV_DUP) {
                    Ok((_k, v)) => val = v,
                    Err(lmdb::Error::NotFound) => {
                        if remaining == 0 {
                            break;
                        }
                        return Err(FinalisedStateError::Custom(format!(
                            "expected {expected} records, deleted {}",
                            expected - remaining
                        )));
                    }
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            },
            Err(lmdb::Error::NotFound) => {
                return Err(FinalisedStateError::Custom(
                    "no addrhist record for key".into(),
                ));
            }
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        }

        drop(cur);
        Ok(())
    }

    /// Mark a specific AddrHistRecord as spent in the addrhist DB.
    /// Looks up a record by script and tx_location, sets FLAG_SPENT, and updates it in place.
    ///
    /// Returns Ok(true) if a record was updated, Ok(false) if not found, or Err on DB error.
    ///
    /// WARNING: This operates *inside* an existing RW txn and must **not** commit it.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(super) fn mark_addr_hist_record_spent_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        addr_script: &AddrScript,

        expected_prev_entry_bytes: &[u8],
    ) -> Result<bool, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        let mut cur = txn.open_rw_cursor(self.address_history)?;

        for (key, val) in cur.iter_dup_of(&addr_bytes)? {
            if key.len() != AddrScript::VERSIONED_LEN {
                return Err(FinalisedStateError::Custom(
                    "address history key length mismatch".into(),
                ));
            }
            if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                return Err(FinalisedStateError::Custom(
                    "address history value length mismatch".into(),
                ));
            }

            if val != expected_prev_entry_bytes {
                continue;
            }

            let mut hist_record = [0u8; StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN];
            hist_record.copy_from_slice(val);

            let flags = hist_record[10];
            if (flags & AddrHistRecord::FLAG_IS_INPUT) != 0 {
                return Err(FinalisedStateError::Custom(
                    "attempt to mark an input-row as spent".into(),
                ));
            }
            // idempotent
            if (flags & AddrHistRecord::FLAG_SPENT) != 0 {
                return Ok(true);
            }

            if (flags & AddrHistRecord::FLAG_MINED) == 0 {
                return Err(FinalisedStateError::Custom(
                    "attempt to mark non-mined addrhist record as spent".into(),
                ));
            }

            hist_record[10] |= AddrHistRecord::FLAG_SPENT;

            let checksum = StoredEntryFixed::<AddrEventBytes>::blake2b256(
                &[&addr_bytes, &hist_record[1..19]].concat(),
            );
            hist_record[19..51].copy_from_slice(&checksum);

            cur.put(&addr_bytes, &hist_record, WriteFlags::CURRENT)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Mark a specific AddrHistRecord as unspent in the addrhist DB.
    /// Looks up a record by script and tx_location, sets FLAG_SPENT, and updates it in place.
    ///
    /// Returns Ok(true) if a record was updated, Ok(false) if not found, or Err on DB error.
    ///
    /// WARNING: This operates *inside* an existing RW txn and must **not** commit it.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(super) fn mark_addr_hist_record_unspent_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        addr_script: &AddrScript,

        expected_prev_entry_bytes: &[u8],
    ) -> Result<bool, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        let mut cur = txn.open_rw_cursor(self.address_history)?;

        for (key, val) in cur.iter_dup_of(&addr_bytes)? {
            if key.len() != AddrScript::VERSIONED_LEN {
                return Err(FinalisedStateError::Custom(
                    "address history key length mismatch".into(),
                ));
            }
            if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                return Err(FinalisedStateError::Custom(
                    "address history value length mismatch".into(),
                ));
            }

            if val != expected_prev_entry_bytes {
                continue;
            }

            // we've located the exact duplicate bytes we built earlier.
            let mut hist_record = [0u8; StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN];
            hist_record.copy_from_slice(val);

            // parse flags (located at byte index 10 in the StoredEntry layout)
            let flags = hist_record[10];

            // Sanity: the record we intend to mark should be a mined output (not an input).
            if (flags & AddrHistRecord::FLAG_IS_INPUT) != 0 {
                return Err(FinalisedStateError::Custom(
                    "attempt to mark an input-row as unspent".into(),
                ));
            }

            // If it's already unspent, treat as successful (idempotent).
            if (flags & AddrHistRecord::FLAG_SPENT) == 0 {
                drop(cur);
                return Ok(true);
            }

            // If the record is not marked MINED, that's an invariant failure.
            // We surface it rather than producing a non-mined record.
            if (flags & AddrHistRecord::FLAG_MINED) == 0 {
                return Err(FinalisedStateError::Custom(
                    "attempt to mark non-mined addrhist record as unspent".into(),
                ));
            }

            // Preserve all existing flags (including MINED), and remove SPENT.
            hist_record[10] &= !AddrHistRecord::FLAG_SPENT;

            // Recompute checksum over entry header + payload (bytes 1..19).
            let checksum = StoredEntryFixed::<AddrEventBytes>::blake2b256(
                &[&addr_bytes, &hist_record[1..19]].concat(),
            );
            hist_record[19..51].copy_from_slice(&checksum);

            // Write back in place for the exact duplicate we matched.
            cur.put(&addr_bytes, &hist_record, WriteFlags::CURRENT)?;
            drop(cur);

            return Ok(true);
        }

        Ok(false)
    }

    /// Fetches the previous transparent output for the given outpoint.
    /// Returns `TxOutCompact` or an explicit error if not found or invalid.
    ///
    /// Used to build addrhist records.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    pub(crate) fn get_previous_output_blocking(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, FinalisedStateError> {
        // Find the tx’s location in the chain
        let prev_txid = TransactionHash::from(*outpoint.prev_txid());
        let tx_location = self
            .find_txid_index_blocking(&prev_txid)?
            .ok_or_else(|| FinalisedStateError::Custom("Previous txid not found".into()))?;

        // Fetch the output from the transparent db.
        let block_height = tx_location.block_height();
        let tx_index = tx_location.tx_index() as usize;
        let out_index = outpoint.prev_index() as usize;

        let ro = self.env.begin_ro_txn()?;
        let height_key = Height(block_height).to_bytes()?;
        let stored_bytes = ro.get(self.transparent, &height_key)?;

        Self::find_txout_in_stored_transparent_tx_list(stored_bytes, tx_index, out_index)
            .ok_or_else(|| {
                FinalisedStateError::Custom("Previous output not found at given index".into())
            })
    }

    /// Efficiently scans a raw `StoredEntryVar<TransparentTxList>` buffer to locate the
    /// specific output at [tx_idx, output_idx] without full deserialization.
    ///
    /// # Arguments
    /// - `stored`: the raw LMDB byte buffer
    /// - `target_tx_idx`: index in the tx list
    /// - `target_output_idx`: index in the outputs of that tx
    ///
    /// # Returns
    /// - `Some(TxOutCompact)` if found and present, otherwise `None`
    #[inline]
    fn find_txout_in_stored_transparent_tx_list(
        stored: &[u8],
        target_tx_idx: usize,
        target_output_idx: usize,
    ) -> Option<TxOutCompact> {
        const CHECKSUM_LEN: usize = 32;

        if stored.len() < TransactionHash::VERSION_TAG_LEN + 8 + CHECKSUM_LEN {
            return None;
        }

        let mut cursor = &stored[TransactionHash::VERSION_TAG_LEN..];
        let item_len = CompactSize::read(&mut cursor).ok()? as usize;
        if cursor.len() < item_len + CHECKSUM_LEN {
            return None;
        }

        let (_record_version, mut remaining) = cursor.split_first()?;
        let vec_len = CompactSize::read(&mut remaining).ok()? as usize;

        for i in 0..vec_len {
            let (option_tag, rest) = remaining.split_first()?;
            remaining = rest;

            if *option_tag == 0 {
                // None: nothing to skip, go to next
                if i == target_tx_idx {
                    return None;
                }
            } else if *option_tag == 1 {
                let (_tx_version, rest) = remaining.split_first()?;
                remaining = rest;

                let vin_len = CompactSize::read(&mut remaining).ok()? as usize;

                for _ in 0..vin_len {
                    if remaining.len() < TxInCompact::VERSIONED_LEN {
                        return None;
                    }
                    remaining = &remaining[TxInCompact::VERSIONED_LEN..];
                }

                let vout_len = CompactSize::read(&mut remaining).ok()? as usize;

                for out_idx in 0..vout_len {
                    if remaining.len() < TxOutCompact::VERSIONED_LEN {
                        return None;
                    }

                    let out_bytes = &remaining[..TxOutCompact::VERSIONED_LEN];

                    if i == target_tx_idx && out_idx == target_output_idx {
                        return TxOutCompact::from_bytes(out_bytes).ok();
                    }

                    remaining = &remaining[TxOutCompact::VERSIONED_LEN..];
                }
            } else {
                // Non-canonical Option tag
                return None;
            }
        }
        None
    }

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
}

impl DbV1 {
    //! *** Bulk txout-set accumulator builder ***
    //!
    //! Replaces the per-block, random-read accumulator maintenance that dominated sync time at
    //! sandblast height. The accumulator over the UTXO set at the current tip is recomputed from
    //! scratch with (almost entirely) sequential scans, exploiting the fact that the
    //! `hash_serialized` field is an XOR multiset commitment: an output created and later spent is
    //! XORed in then out and cancels, so the live set is exactly the created-and-not-spent outputs.

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

        tokio::task::block_in_place(|| {
            let accumulator =
                self.build_tx_out_set_accumulator_blocking(db_tip, ACCUMULATOR_BUILD_SHARDS)?;

            let mut txn = self.env.begin_rw_txn()?;

            let accumulator_entry =
                StoredEntryFixed::new(TX_OUT_SET_INFO_ACCUMULATOR_KEY, accumulator);
            txn.put(
                self.tx_out_set_info_accumulator,
                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                &accumulator_entry.to_bytes()?,
                WriteFlags::empty(),
            )?;

            let watermark = StoredEntryFixed::new(TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY, db_tip);
            txn.put(
                self.metadata,
                &TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
                &watermark.to_bytes()?,
                WriteFlags::empty(),
            )?;

            txn.commit()?;
            self.env.sync(true)?;

            Ok::<_, FinalisedStateError>(())
        })
    }

    /// Computes the finalised txout-set accumulator over the UTXO set at `db_tip`.
    ///
    /// Strategy (per shard): scan the `spent` table once to collect the spent outpoints whose
    /// creating txid falls in the shard, then scan the block `transparent` + `txids` tables in
    /// ascending height order, adding every spendable output that is not in that spent set. The
    /// `transactions` count is derived locally per transaction (all of a tx's outputs live in one
    /// height entry). Sharding bounds the in-memory spent set; partials recombine exactly.
    ///
    /// WARNING: blocking — call from a blocking context. Builds to `db_tip` only (the spent table
    /// is assumed to cover spends up to the same tip).
    pub(crate) fn build_tx_out_set_accumulator_blocking(
        &self,
        db_tip: Height,
        shards: u16,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        let shards = shards.max(1) as usize;
        let mut total = FinalisedTxOutSetInfoAccumulator::empty();

        for shard in 0..shards {
            // First-byte range [lo, hi) of the creating-txid assigned to this shard.
            let lo = (shard * 256 / shards) as u16;
            let hi = ((shard + 1) * 256 / shards) as u16;
            let in_shard = |first_byte: u8| -> bool {
                let b = first_byte as u16;
                b >= lo && b < hi
            };

            // One read snapshot for the whole shard pass (subsumes the per-lookup RO-txn churn the
            // old per-block path incurred).
            let txn = self.env.begin_ro_txn()?;

            // (1) Spent outpoints in this shard. The `spent` key is `Outpoint::to_bytes()` =
            //     `[version tag][32-byte prev_txid][4-byte index]`, so the prev-txid's first byte
            //     (which equals the creating txid's first byte) is at index 1.
            let mut spent_set: HashSet<Box<[u8]>> = HashSet::new();
            {
                let mut cursor = txn.open_ro_cursor(self.spent)?;
                for (key_bytes, _value) in cursor.iter() {
                    if key_bytes.len() < 2 || !in_shard(key_bytes[1]) {
                        continue;
                    }
                    spent_set.insert(Box::from(key_bytes));
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

            // Recombine: XOR the multiset commitments, sum the additive counters.
            for (dst, src) in total
                .hash_serialized
                .iter_mut()
                .zip(shard_acc.hash_serialized.iter())
            {
                *dst ^= *src;
            }
            total.transactions = total
                .transactions
                .checked_add(shard_acc.transactions)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator transactions overflow".to_string(),
                    )
                })?;
            total.transaction_outputs = total
                .transaction_outputs
                .checked_add(shard_acc.transaction_outputs)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator transaction_outputs overflow".to_string(),
                    )
                })?;
            total.bytes_serialized = total
                .bytes_serialized
                .checked_add(shard_acc.bytes_serialized)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator bytes_serialized overflow".to_string(),
                    )
                })?;
            total.total_zatoshis = total
                .total_zatoshis
                .checked_add(shard_acc.total_zatoshis)
                .ok_or_else(|| {
                    FinalisedStateError::Custom(
                        "txout-set accumulator total_zatoshis overflow".to_string(),
                    )
                })?;
        }

        Ok(total)
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

                    for input in transparent_tx.inputs().iter() {
                        if input.is_null_prevout() {
                            continue;
                        }
                        spends.push(Outpoint::new(*input.prevout_txid(), input.prevout_index()));
                    }
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

            let accumulator_entry =
                StoredEntryFixed::new(TX_OUT_SET_INFO_ACCUMULATOR_KEY, accumulator);
            txn.put(
                self.tx_out_set_info_accumulator,
                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                &accumulator_entry.to_bytes()?,
                WriteFlags::empty(),
            )?;

            let watermark = StoredEntryFixed::new(TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY, tip);
            txn.put(
                self.metadata,
                &TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
                &watermark.to_bytes()?,
                WriteFlags::empty(),
            )?;

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
        Ok(Self::find_txout_in_stored_transparent_tx_list(
            stored,
            location.tx_index() as usize,
            outpoint.prev_index() as usize,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::types::db::metadata::{
        FinalisedTxOutSetInfoAccumulator, ZAINO_TXOUTSET_ENTRY_LEN,
    };

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
}
