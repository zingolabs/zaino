//! FinalisedState::V1 transparent address history indexing functionality.

use crate::types::db::metadata::FinalisedTxOutSetInfoAccumulator;

use super::*;
#[cfg(feature = "transparent_address_history_experimental")]
use crate::store::capability::AddrUtxo;

/// Decodes one stored address-history value into its record.
#[cfg(feature = "transparent_address_history_experimental")]
fn decode_addr_event(val: &[u8]) -> Result<AddrHistRecord, StoreError> {
    AddrEventBytes::from_bytes(val)
        .and_then(|event| event.as_record())
        .map_err(|e| StoreError::Custom(format!("addrhist decode error: {e}")))
}

/// Encodes one address-history record as its stored value.
#[cfg(feature = "transparent_address_history_experimental")]
fn encode_addr_event(record: &AddrHistRecord) -> Result<Vec<u8>, StoreError> {
    Ok(AddrEventBytes::from_record(record)?.to_bytes()?)
}

/// Which way a stored output's spent flag moves.
#[cfg(feature = "transparent_address_history_experimental")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpentMark {
    /// The output is spent by an input being written.
    Spent,
    /// The output's spend is unwound by a block being deleted.
    Unspent,
}

#[cfg(feature = "transparent_address_history_experimental")]
impl SpentMark {
    /// The word an error names this mark by.
    fn name(self) -> &'static str {
        match self {
            Self::Spent => "spent",
            Self::Unspent => "unspent",
        }
    }

    /// Whether `record` already carries this mark.
    fn is_applied(self, record: &AddrHistRecord) -> bool {
        match self {
            Self::Spent => record.is_spent(),
            Self::Unspent => !record.is_spent(),
        }
    }

    /// `flags` with this mark applied.
    fn apply(self, flags: u8) -> u8 {
        match self {
            Self::Spent => flags | AddrHistRecord::FLAG_SPENT,
            Self::Unspent => flags & !AddrHistRecord::FLAG_SPENT,
        }
    }
}

/// [`TransparentHistExt`] capability implementation for [`DbV1`].
///
/// Provides address history queries built over the LMDB `DUP_SORT`/`DUP_FIXED` address-history
/// database.
#[cfg(feature = "transparent_address_history_experimental")]
impl TransparentHistExt for DbV1 {
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
        self.addr_records(addr_script).await
    }

    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
        self.addr_and_index_records(addr_script, tx_location).await
    }

    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, StoreError> {
        self.addr_tx_locations_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<AddrUtxo>>, StoreError> {
        self.addr_utxos_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, StoreError> {
        self.addr_balance_by_range(addr_script, start_height, end_height)
            .await
    }
}

/// [`SpentOutputExt`] capability implementation for [`DbV1`].
impl SpentOutputExt for DbV1 {
    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, StoreError> {
        self.get_outpoint_spender(outpoint).await
    }

    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, StoreError> {
        self.get_outpoint_spenders(outpoints).await
    }
}

/// [`TxOutSetExt`] capability implementation for [`DbV1`].
impl TxOutSetExt for DbV1 {
    async fn get_tx_out_set_info_accumulator(
        &self,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, StoreError> {
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
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            let mut raw_records = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::ENCODED_LEN {
                    continue;
                }
                if val.len() != AddrEventBytes::ENCODED_LEN {
                    continue;
                }
                raw_records.push(val.to_vec());
            }

            if raw_records.is_empty() {
                return Ok(None);
            }

            let mut records = Vec::with_capacity(raw_records.len());
            for val in raw_records {
                records.push(
                    AddrEventBytes::from_bytes(&val)
                        .map_err(|e| StoreError::Custom(format!("addrhist decode error: {e}")))?,
                );
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
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
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
            Err(StoreError::LmdbError(lmdb::Error::NotFound)) => return Ok(None),
            Err(e) => return Err(e),
        };

        if raw_records.is_empty() {
            return Ok(None);
        }

        let mut records = Vec::with_capacity(raw_records.len());

        for val in raw_records {
            records.push(
                AddrEventBytes::from_bytes(&val)
                    .map_err(|e| StoreError::Custom(format!("addrhist decode error: {e}")))?,
            );
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
    ) -> Result<Option<Vec<TxLocation>>, StoreError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let mut set: HashSet<TxLocation> = HashSet::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::ENCODED_LEN || val.len() != AddrEventBytes::ENCODED_LEN
                {
                    continue;
                }

                let record = decode_addr_event(val)?;
                let block_height = record.tx_location().block_height();
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                set.insert(record.tx_location());
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
    ) -> Result<Option<Vec<AddrUtxo>>, StoreError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let mut utxos = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::ENCODED_LEN || val.len() != AddrEventBytes::ENCODED_LEN
                {
                    continue;
                }

                let record = decode_addr_event(val)?;
                let block_height = record.tx_location().block_height();
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                if !record.is_mined() || record.is_spent() {
                    continue;
                }

                utxos.push((record.tx_location(), record.out_index(), record.value()));
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
    ) -> Result<i64, StoreError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            let mut balance: i64 = 0;

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::ENCODED_LEN || val.len() != AddrEventBytes::ENCODED_LEN
                {
                    continue;
                }

                let record = decode_addr_event(val)?;
                let height = record.tx_location().block_height();
                if height < start_height.0 || height > end_height.0 {
                    continue;
                }

                let value = record.value() as i64;
                if record.is_input() {
                    balance -= value;
                } else if record.is_mined() {
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
    ) -> Result<Option<TxLocation>, StoreError> {
        let key = outpoint.to_bytes()?;
        let txn = self.env.begin_ro_txn()?;

        tokio::task::block_in_place(|| match txn.get(self.spent, &key) {
            Ok(bytes) => TxLocation::from_bytes(bytes)
                .map(Some)
                .map_err(|e| StoreError::Custom(format!("spent entry decode error: {e}"))),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(StoreError::LmdbError(e)),
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
    ) -> Result<Vec<Option<TxLocation>>, StoreError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            outpoints
                .into_iter()
                .map(|outpoint| {
                    let key = outpoint.to_bytes()?;
                    match txn.get(self.spent, &key) {
                        Ok(bytes) => TxLocation::from_bytes(bytes).map(Some).map_err(|e| {
                            StoreError::Custom(format!(
                                "spent entry decode error for {outpoint:?}: {e}"
                            ))
                        }),
                        Err(lmdb::Error::NotFound) => Ok(None),
                        Err(e) => Err(StoreError::LmdbError(e)),
                    }
                })
                .collect()
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
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        // Open a single cursor.
        let cursor = txn.open_ro_cursor(self.address_history)?;
        let mut results: Vec<Vec<u8>> = Vec::new();

        // Build the SET_RANGE value prefix that matches the stored bytes:
        //
        //  - 4 bytes: block_height  (big-endian)
        //  - 2 bytes: tx_index     (big-endian)
        //
        // This prefix is all MDB_SET_RANGE needs to position at the first duplicate whose value
        // is >= (height, tx_index); vout, flags and value follow it in the stored bytes.
        let mut seek_data = Vec::with_capacity(4 + 2);
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
                if cur_key.len() != AddrScript::ENCODED_LEN
                    || &cur_key[..AddrScript::ENCODED_LEN] != addr_script_bytes
                {
                    return Ok(results);
                }

                // Iterate from the positioned duplicate forward using MDB_NEXT_DUP.
                let op_next_dup = lmdb_sys::MDB_NEXT_DUP;

                loop {
                    // Validate lengths, same as original function.
                    if cur_key.len() != AddrScript::ENCODED_LEN {
                        return Err(StoreError::Custom(
                            "address history key length mismatch".into(),
                        ));
                    }
                    if cur_val.len() != AddrEventBytes::ENCODED_LEN {
                        return Err(StoreError::Custom(
                            "address history value length mismatch".into(),
                        ));
                    }

                    let record_location = decode_addr_event(cur_val)?.tx_location();
                    let block_index = record_location.block_height();
                    let tx_idx = record_location.tx_index();

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
                            if k.len() != AddrScript::ENCODED_LEN
                                || &k[..AddrScript::ENCODED_LEN] != addr_script_bytes
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
    ) -> Result<(), StoreError> {
        if !delete_inputs && !delete_outputs {
            return Err(StoreError::Custom(
                "called delete_addrhist_dups with neither inputs nor outputs to delete".into(),
            ));
        }
        if expected == 0 {
            return Err(StoreError::Custom(
                "called delete_addrhist_dups with 0 expected deletes".into(),
            ));
        }

        let mut remaining = expected;

        let mut cur = txn.open_rw_cursor(self.address_history)?;

        match cur
            .get(Some(addr_bytes), None, lmdb_sys::MDB_SET_KEY)
            .and_then(|_| cur.get(None, None, lmdb_sys::MDB_LAST_DUP))
        {
            Ok((_k, mut val)) => loop {
                if val.len() != AddrEventBytes::ENCODED_LEN {
                    tracing::warn!("bad addrhist dup (len={})", val.len());
                } else {
                    let record = decode_addr_event(val)?;
                    if record.tx_location().block_height() == block_height.0
                        && ((delete_inputs && record.is_input())
                            || (delete_outputs && record.is_mined()))
                    {
                        cur.del(WriteFlags::empty())?;
                        remaining -= 1;
                        if remaining == 0 {
                            break;
                        }
                    }
                }

                // step backwards through duplicates
                match cur.get(None, None, lmdb_sys::MDB_PREV_DUP) {
                    Ok((_k, v)) => val = v,
                    Err(lmdb::Error::NotFound) => {
                        if remaining == 0 {
                            break;
                        }
                        return Err(StoreError::Custom(format!(
                            "expected {expected} records, deleted {}",
                            expected - remaining
                        )));
                    }
                    Err(e) => return Err(StoreError::LmdbError(e)),
                }
            },
            Err(lmdb::Error::NotFound) => {
                return Err(StoreError::Custom("no addrhist record for key".into()));
            }
            Err(e) => return Err(StoreError::LmdbError(e)),
        }

        drop(cur);
        Ok(())
    }

    /// Rewrites in place, inside the caller's transaction, the mined output record of `addr_script` whose stored bytes equal `expected_prev_entry_bytes` with `mark` applied, answering whether one was found.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(super) fn mark_addr_hist_record_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        addr_script: &AddrScript,
        expected_prev_entry_bytes: &[u8],
        mark: SpentMark,
    ) -> Result<bool, StoreError> {
        let addr_bytes = addr_script.to_bytes()?;

        let mut cur = txn.open_rw_cursor(self.address_history)?;

        for (key, val) in cur.iter_dup_of(&addr_bytes)? {
            if key.len() != AddrScript::ENCODED_LEN {
                return Err(StoreError::Custom(
                    "address history key length mismatch".into(),
                ));
            }
            if val.len() != AddrEventBytes::ENCODED_LEN {
                return Err(StoreError::Custom(
                    "address history value length mismatch".into(),
                ));
            }

            if val != expected_prev_entry_bytes {
                continue;
            }

            let record = decode_addr_event(val)?;
            if record.is_input() {
                return Err(StoreError::Custom(format!(
                    "attempt to mark an input-row as {}",
                    mark.name()
                )));
            }
            // idempotent
            if mark.is_applied(&record) {
                return Ok(true);
            }
            if !record.is_mined() {
                return Err(StoreError::Custom(format!(
                    "attempt to mark non-mined addrhist record as {}",
                    mark.name()
                )));
            }

            let marked = AddrHistRecord::new(
                record.tx_location(),
                record.out_index(),
                record.value(),
                mark.apply(record.flags()),
            );
            cur.put(
                &addr_bytes,
                &encode_addr_event(&marked)?,
                WriteFlags::CURRENT,
            )?;
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
    ) -> Result<TxOutCompact, StoreError> {
        // Find the tx’s location in the chain
        let prev_txid = TransactionHash::from(*outpoint.prev_txid());
        let tx_location = self
            .find_txid_index_blocking(&prev_txid)?
            .ok_or_else(|| StoreError::Custom("Previous txid not found".into()))?;

        // Fetch the output from the transparent db.
        let block_height = tx_location.block_height();
        let tx_index = tx_location.tx_index() as usize;
        let out_index = outpoint.prev_index() as usize;

        let ro = self.env.begin_ro_txn()?;
        let height_key = Height(block_height).to_bytes()?;
        let stored_bytes = ro.get(self.transparent, &height_key)?;

        Self::find_txout_in_stored_transparent_tx_list(stored_bytes, tx_index, out_index)?
            .ok_or_else(|| StoreError::Custom("Previous output not found at given index".into()))
    }

    /// Efficiently scans a raw stored `TransparentTxList` buffer to locate the
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
    pub(super) fn find_txout_in_stored_transparent_tx_list(
        stored: &[u8],
        target_tx_idx: usize,
        target_output_idx: usize,
    ) -> Result<Option<TxOutCompact>, StoreError> {
        let mut remaining = stored;
        let vec_len = CompactSize::read(&mut remaining)? as usize;

        for i in 0..vec_len {
            let Some((option_tag, rest)) = remaining.split_first() else {
                return Ok(None);
            };
            remaining = rest;

            if *option_tag == 0 {
                // None: nothing to skip, go to next
                if i == target_tx_idx {
                    return Ok(None);
                }
            } else if *option_tag == 1 {
                let vin_len = CompactSize::read(&mut remaining)? as usize;

                for _ in 0..vin_len {
                    if remaining.len() < TxInCompact::ENCODED_LEN {
                        return Ok(None);
                    }
                    remaining = &remaining[TxInCompact::ENCODED_LEN..];
                }

                let vout_len = CompactSize::read(&mut remaining)? as usize;

                for out_idx in 0..vout_len {
                    if remaining.len() < TxOutCompact::ENCODED_LEN {
                        return Ok(None);
                    }

                    let out_bytes = &remaining[..TxOutCompact::ENCODED_LEN];

                    if i == target_tx_idx && out_idx == target_output_idx {
                        return Ok(TxOutCompact::from_bytes(out_bytes).ok());
                    }

                    remaining = &remaining[TxOutCompact::ENCODED_LEN..];
                }
            } else {
                // Non-canonical Option tag
                return Ok(None);
            }
        }
        Ok(None)
    }
}
