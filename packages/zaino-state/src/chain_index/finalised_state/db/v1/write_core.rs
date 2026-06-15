//! ZainoDB::V1 core write functionality.

use super::*;
use crate::chain_index::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
use crate::chain_index::types::Height;
#[cfg(test)]
use crate::version;

/// [`DbWrite`] capability implementation for [`DbV1`].
///
/// This trait represents the mutating surface (append / delete tip / update metadata). Writes are
/// performed via LMDB write transactions. The write path does not validate committed data; the
/// background validator was removed.
#[async_trait]
impl DbWrite for DbV1 {
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block(block).await
    }

    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError> {
        self.delete_block_at_height(height).await
    }

    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.delete_block(block).await
    }

    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        self.update_metadata(metadata).await
    }
}

impl DbV1 {
    //! *** DB write / delete methods ***
    //! **These should only ever be used in a single DB control task.**

    /// Writes a given (finalised) [`IndexedBlock`] to ZainoDB.
    ///
    /// NOTE: This method should never leave a block partially written to the database.
    // `u32::is_multiple_of` is only stable from Rust 1.87; the `% 100 == 0` form below keeps the
    // crate buildable on our older minimum supported Rust version.
    #[allow(clippy::manual_is_multiple_of)]
    pub(crate) async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);
        let block_hash = block.context.index.hash;
        let block_height = block.context.index.height;
        let block_height_bytes = block_height.to_bytes()?;

        // Check if this specific block already exists (idempotent write support for shared DB).
        // This handles the case where multiple processes share the same ZainoDB.
        let block_already_exists = tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;

            // First, check if a block at this specific height already exists
            match ro.get(self.headers, &block_height_bytes) {
                Ok(stored_header_bytes) => {
                    // Block exists at this height - verify it's the same block
                    // Data is stored as StoredEntryVar<BlockHeaderData>, so deserialize properly
                    let stored_entry =
                        StoredEntryVar::<BlockHeaderData>::from_bytes(stored_header_bytes)
                            .map_err(|e| {
                                FinalisedStateError::Custom(format!(
                                    "header decode error during idempotency check: {e}"
                                ))
                            })?;
                    let stored_header = stored_entry.inner();
                    if stored_header.context.index.hash == block_hash {
                        // Same block already written, this is a no-op success
                        return Ok(true);
                    } else {
                        return Err(FinalisedStateError::Custom(format!(
                            "block at height {block_height:?} already exists with different hash \
                             (stored: {:?}, incoming: {:?})",
                            stored_header.context.index.hash, block_hash
                        )));
                    }
                }
                Err(lmdb::Error::NotFound) => {
                    // Block doesn't exist at this height, check if it's the next in sequence
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }

            // Now verify this is the next block in the chain
            let cur = ro.open_ro_cursor(self.headers)?;
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                // Database already has blocks
                Ok((last_height_bytes, _last_header_bytes)) => {
                    let last_height = Height::from_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?;

                    // Height must be exactly +1 over the current tip
                    if block_height.0 != last_height.0 + 1 {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot write block at height {block_height:?}; \
                     current tip is {last_height:?}"
                        )));
                    }
                }
                // no block in db, this must be genesis block.
                Err(lmdb::Error::NotFound) => {
                    if block_height.0 != GENESIS_HEIGHT.0 {
                        return Err(FinalisedStateError::Custom(format!(
                            "first block must be height 0, got {block_height:?}"
                        )));
                    }
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }
            Ok::<_, FinalisedStateError>(false)
        })?;

        // If block already exists with same hash, return success without re-writing
        if block_already_exists {
            self.status.store(StatusType::Ready);
            info!(
                "Block {} at height {} already exists in ZainoDB, skipping write.",
                &block_hash, &block_height.0
            );
            // Another process is writing this database; its spends are invisible
            // to our in-memory counts.
            self.invalidate_unspent_output_counts();
            return Ok(());
        }

        let data = self.build_block_write_data(&block, None).await?;

        // if any database writes fail, remove block from database and return err.
        let zaino_db = self.task_clone();
        let join_handle = tokio::task::spawn_blocking(move || {
            // Write block to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            zaino_db.put_block_write_data_in_txn(&mut txn, data)?;

            // `txn.commit()` is durable: the LMDB env is opened without NO_SYNC, so commit
            // fsyncs the data and meta pages — atomicity and durability come from LMDB.
            //
            // The write path does not validate: the background validator was removed, so
            // committed records are served without a read-back/re-hash pass.
            txn.commit()?;

            Ok::<_, FinalisedStateError>(())
        });

        // Wait for the join and handle panic / cancellation explicitly so we can
        // attempt to remove any partially written block.
        let post_result = match join_handle.await {
            Ok(inner_res) => inner_res,
            Err(join_err) => {
                warn!("Tokio task error (spawn_blocking join error): {}", join_err);

                // Best-effort delete of partially written block; ignore delete result.
                let _ = self.delete_block(&block).await;
                self.invalidate_unspent_output_counts();

                return Err(FinalisedStateError::Custom(format!(
                    "Tokio task error: {}",
                    join_err
                )));
            }
        };

        match post_result {
            Ok(_) => {
                // The block (and its `txid_location` entries) were durably committed inside the
                // blocking task above. The write path does not validate; the background validator
                // was removed.
                self.status.store(StatusType::Ready);
                if block.context.index.height.0 % 100 == 0 {
                    info!(
                        "Successfully committed block {} at height {} to ZainoDB.",
                        &block.context.index.hash, &block.context.index.height
                    );
                } else {
                    tracing::debug!(
                        "Successfully committed block {} at height {} to ZainoDB.",
                        &block.context.index.hash,
                        &block.context.index.height
                    );
                }

                Ok(())
            }
            Err(FinalisedStateError::LmdbError(lmdb::Error::KeyExist)) => {
                // Block write failed because key already exists - another process wrote it
                // between our check and our write.
                self.invalidate_unspent_output_counts();
                //
                // Wait briefly and verify it's the same block and was fully written to the finalised state.
                // Partially written block should be deleted from the database and the write error reported
                // so the on disk tables are never corrupted by a partial block writes.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                let height_bytes = block_height.to_bytes()?;
                let verification_result = tokio::task::block_in_place(|| {
                    // Sync to see latest commits from other processes
                    self.env.sync(true).ok();
                    let ro = self.env.begin_ro_txn()?;
                    match ro.get(self.headers, &height_bytes) {
                        Ok(stored_header_bytes) => {
                            // Data is stored as StoredEntryVar<BlockHeaderData>
                            let stored_entry =
                                StoredEntryVar::<BlockHeaderData>::from_bytes(stored_header_bytes)
                                    .map_err(|e| {
                                        FinalisedStateError::Custom(format!(
                                            "header decode error in KeyExist handler: {e}"
                                        ))
                                    })?;
                            let stored_header = stored_entry.inner();
                            if stored_header.context.index.hash == block_hash {
                                // Block already present with the expected hash: the prior
                                // write committed it. Treat as an idempotent success.
                                Ok(true)
                            } else {
                                Err(FinalisedStateError::Custom(format!(
                                    "KeyExist race: different block at height {} \
                                     (stored: {:?}, incoming: {:?})",
                                    block_height.0, stored_header.context.index.hash, block_hash
                                )))
                            }
                        }
                        Err(lmdb::Error::NotFound) => Err(FinalisedStateError::Custom(format!(
                            "KeyExist but block not found at height {} after sync",
                            block_height.0
                        ))),
                        Err(e) => Err(FinalisedStateError::LmdbError(e)),
                    }
                });

                match verification_result {
                    Ok(_) => {
                        // Block already written by another process; our build-time cache
                        // update may not match the shared DB, so reseed from committed state.
                        self.reseed_transparent_utxo_cache()?;
                        self.status.store(StatusType::Ready);
                        info!(
                            "Block {} at height {} was already written by another process, skipping.",
                            &block_hash, &block_height.0
                        );
                        Ok(())
                    }
                    Err(e) => {
                        warn!("Error writing block to DB: {e}");
                        warn!(
                            "Deleting corrupt block from DB at height: {} with hash: {:?}",
                            block_height.0, block_hash.0
                        );

                        let _ = self.delete_block(&block).await;
                        tokio::task::block_in_place(|| self.env.sync(true)).map_err(|e| {
                            FinalisedStateError::Custom(format!("LMDB sync failed: {e}"))
                        })?;
                        self.status.store(StatusType::CriticalError);
                        self.status.store(StatusType::RecoverableError);
                        Err(FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: e.to_string(),
                        })
                    }
                }
            }
            Err(e) => {
                self.invalidate_unspent_output_counts();

                if matches!(e, FinalisedStateError::LmdbError(lmdb::Error::MapFull)) {
                    // The transaction aborted atomically: nothing was committed and the
                    // database is intact (only the size cap was reached), but the build
                    // phase already applied this block to the cache — reseed.
                    self.reseed_transparent_utxo_cache()?;
                    self.status.store(StatusType::RecoverableError);
                    return Err(self.map_full_config_error());
                }

                warn!("Error writing block to DB: {e}");
                warn!(
                    "Deleting corrupt block from DB at height: {} with hash: {:?}",
                    block_height.0, block_hash.0
                );

                let _ = self.delete_block(&block).await;
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;

                // NOTE: this does not need to be critical if we implement self healing,
                // which we have the tools to do.
                self.status.store(StatusType::CriticalError);

                Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Builds everything needed to persist `block`: encoded key bytes, checksummed table
    /// entries, per-block index maps, and the post-block txout-set accumulator.
    ///
    /// `pending` is the in-memory overlay of an open write batch ([`PendingBatchState`]):
    /// every read that may touch state written by an earlier, uncommitted batch block —
    /// transparent prevout resolution, spent-output checks, and the accumulator itself —
    /// consults the overlay before the committed tables. Pass `None` on the single-block
    /// write path, where all prior state is committed.
    async fn build_block_write_data(
        &self,
        block: &IndexedBlock,
        pending: Option<&PendingBatchState>,
    ) -> Result<BlockWriteData, FinalisedStateError> {
        let block_hash = block.context.index.hash;
        let block_hash_bytes = block_hash.to_bytes()?;
        let block_height = block.context.index.height;
        let block_height_bytes = block_height.to_bytes()?;

        // Build DBHeight
        let height_entry_bytes =
            StoredEntryFixed::encode(&block_hash_bytes, &block.context.index.height)?;

        // Build header
        let header_entry_bytes = StoredEntryVar::encode(
            &block_height_bytes,
            &BlockHeaderData::new(block.context, *block.data()),
        )?;

        // Build commitment tree data
        let commitment_tree_entry_bytes =
            StoredEntryFixed::encode(&block_height_bytes, block.commitment_tree_data())?;

        // Build transaction indexes.
        //
        // `transactions` pairs each transaction hash with its transparent data. Both halves
        // are sourced from the same `tx` in the loop below, so misalignment is structurally
        // impossible — the pair shares one binding. Downstream the accumulator consumes the
        // paired slice; for storage we `unzip` into the existing `TxidList` / `TransparentTxList`
        // shapes.
        let tx_len = block.transactions().len();
        let mut transactions: Vec<(TransactionHash, Option<TransparentCompactTx>)> =
            Vec::with_capacity(tx_len);
        // txid -> in-block index. `insert` returning `Some` is the duplicate-txid
        // guard; the index also makes in-block prevout lookups O(1).
        let mut txid_index: HashMap<TransactionHash, u16> = HashMap::with_capacity(tx_len);
        // Reverse txid index entries (`txid -> TxLocation`), sorted before the write
        // txn so the random-keyed `txid_location` B-tree sees locally-ordered inserts.
        let mut txid_location_entries: Vec<([u8; 32], Vec<u8>)> = Vec::with_capacity(tx_len);
        let mut sapling = Vec::with_capacity(tx_len);
        let mut orchard = Vec::with_capacity(tx_len);

        #[cfg(feature = "transparent_address_history_experimental")]
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            // Bound the index first: the dup map, the reverse-index entry, and the
            // spent map all want the narrow u16 form.
            let tx_index =
                u16::try_from(tx_index).map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("transaction index {tx_index} does not fit into u16"),
                })?;
            let tx_location = TxLocation::new(block_height.into(), tx_index);

            if txid_index.insert(*hash, tx_index).is_some() {
                return Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("duplicate transaction hash in block: {hash:?}"),
                });
            }

            let txid_bytes: [u8; 32] = (*hash).into();
            txid_location_entries.push((
                txid_bytes,
                StoredEntryFixed::encode(&txid_bytes, &tx_location)?,
            ));

            // Transparent transactions — paired with the txid at the source binding.
            let transparent_data = stored_transparent_data(tx);
            transactions.push((*hash, transparent_data));

            // Sapling transactions
            let sapling_data = stored_sapling_data(tx);
            sapling.push(sapling_data);

            // Orchard transactions
            let orchard_data = stored_orchard_data(tx);
            orchard.push(orchard_data);

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Transparent Outputs: Build Address History
                DbV1::build_transaction_output_histories(
                    &mut addrhist_outputs_map,
                    tx_location,
                    tx.transparent().outputs().iter().enumerate(),
                );

                // Transparent Inputs: Build Address History
                for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                    if input.is_null_prevout() {
                        continue;
                    }
                    let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                    // Check if output is in *this* block, else fetch from DB.
                    let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                    if let Some(&prev_idx) = txid_index.get(&prev_tx_hash) {
                        // In-bounds by construction: `prev_idx` was assigned when that
                        // transaction was pushed into `transactions`, and the current
                        // transaction is pushed before its inputs are processed.
                        if let (_, Some(prev_transparent)) = &transactions[prev_idx as usize] {
                            // Fetch output from transaction
                            if let Some(prev_output) = prev_transparent
                                .outputs()
                                .get(prev_outpoint.prev_index() as usize)
                            {
                                let prev_output_tx_location =
                                    TxLocation::new(block_height.0, prev_idx);
                                DbV1::build_input_history(
                                    &mut addrhist_inputs_map,
                                    tx_location,
                                    input_index as u16,
                                    input,
                                    prev_output,
                                    prev_output_tx_location,
                                );
                            }
                        }
                    } else if let Some((prev_location, Some(prev_transparent))) =
                        pending.and_then(|batch| batch.transactions.get(&prev_tx_hash))
                    {
                        // Prevout created by an uncommitted batch block: resolve from
                        // the overlay (the committed tables don't have it yet).
                        if let Some(prev_output) = prev_transparent
                            .outputs()
                            .get(prev_outpoint.prev_index() as usize)
                        {
                            DbV1::build_input_history(
                                &mut addrhist_inputs_map,
                                tx_location,
                                input_index as u16,
                                input,
                                prev_output,
                                *prev_location,
                            );
                        } else {
                            return Err(FinalisedStateError::InvalidBlock {
                                height: block.height().0,
                                hash: *block.hash(),
                                reason: "Invalid block data: invalid transparent input."
                                    .to_string(),
                            });
                        }
                    } else if let Ok((prev_output, prev_output_tx_location)) =
                        tokio::task::block_in_place(|| {
                            let prev_output = self.get_previous_output_blocking(prev_outpoint)?;
                            let prev_output_tx_location = self
                                .find_txid_index_blocking(&TransactionHash::from(
                                    *prev_outpoint.prev_txid(),
                                ))?
                                .ok_or_else(|| {
                                    FinalisedStateError::Custom("Previous txid not found".into())
                                })?;
                            Ok::<(_, _), FinalisedStateError>((
                                prev_output,
                                prev_output_tx_location,
                            ))
                        })
                    {
                        DbV1::build_input_history(
                            &mut addrhist_inputs_map,
                            tx_location,
                            input_index as u16,
                            input,
                            &prev_output,
                            prev_output_tx_location,
                        );
                    } else {
                        return Err(FinalisedStateError::InvalidBlock {
                            height: block.height().0,
                            hash: *block.hash(),
                            reason: "Invalid block data: invalid transparent input.".to_string(),
                        });
                    }
                }
            }
        }

        // Derive the block's transparent delta once; the spent index (here), the
        // accumulator, and (in later stages) the UTXO cache all consume it instead of
        // re-walking the transactions.
        let transparent_delta =
            transparent_delta::block_transparent_delta(block_height, &transactions)?;
        let spent_map = transparent_delta::spent_map_from_delta(&transparent_delta);

        let tx_out_set_info_accumulator = self
            .calculate_tx_out_set_info_accumulator_after_block(
                block_height,
                &transactions,
                &spent_map,
                pending,
            )
            .await?;

        // Maintain the in-memory UTXO cache at build time, *after* this block's
        // accumulator has read the pre-block cache, so that within a batch the next
        // block's accumulator sees this block's transparent state. A failed or aborted
        // commit reseeds the cache (write_block / write_blocks), so this build-time
        // update never outlives a write that did not durably land.
        self.transparent_utxo_cache.apply_forward(&transparent_delta);

        // Split the paired vector into the per-table shapes used for storage.
        let (txids, transparent): (Vec<TransactionHash>, Vec<Option<TransparentCompactTx>>) =
            transactions.into_iter().unzip();

        txid_location_entries.sort_by_key(|entry| entry.0);

        let txid_entry_bytes = StoredEntryVar::encode(&block_height_bytes, &TxidList::new(txids))?;
        let transparent_entry_bytes =
            StoredEntryVar::encode(&block_height_bytes, &TransparentTxList::new(transparent))?;
        let sapling_entry_bytes =
            StoredEntryVar::encode(&block_height_bytes, &SaplingTxList::new(sapling))?;
        let orchard_entry_bytes =
            StoredEntryVar::encode(&block_height_bytes, &OrchardTxList::new(orchard))?;

        // Pre-encode the spent-index and accumulator entries too, so the write
        // transaction performs no serialization at all.
        let mut spent_entries = Vec::with_capacity(spent_map.len());
        for (outpoint, tx_location) in &spent_map {
            let outpoint_bytes = outpoint.to_bytes()?;
            let entry_bytes = StoredEntryFixed::encode(&outpoint_bytes, tx_location)?;
            spent_entries.push((outpoint_bytes, entry_bytes));
        }
        let tx_out_set_info_accumulator_entry_bytes = StoredEntryFixed::encode(
            TX_OUT_SET_INFO_ACCUMULATOR_KEY,
            &tx_out_set_info_accumulator,
        )?;

        Ok(BlockWriteData {
            block_hash,
            block_height,
            block_hash_bytes,
            block_height_bytes,
            height_entry_bytes,
            header_entry_bytes,
            commitment_tree_entry_bytes,
            txid_location_entries,
            txid_entry_bytes,
            transparent_entry_bytes,
            sapling_entry_bytes,
            orchard_entry_bytes,
            spent_entries,
            spent_map,
            tx_out_set_info_accumulator,
            tx_out_set_info_accumulator_entry_bytes,
            #[cfg(feature = "transparent_address_history_experimental")]
            addrhist_inputs_map,
            #[cfg(feature = "transparent_address_history_experimental")]
            addrhist_outputs_map,
        })
    }

    /// Packs one address-history record and encodes its stored entry under
    /// `addr_bytes` in a single pass.
    #[cfg(feature = "transparent_address_history_experimental")]
    fn encode_addr_hist_entry(
        addr_bytes: &[u8],
        record: &AddrHistRecord,
    ) -> Result<Vec<u8>, FinalisedStateError> {
        let packed = AddrEventBytes::from_record(record).map_err(|e| {
            FinalisedStateError::Custom(format!("AddrEventBytes pack error: {e:?}"))
        })?;
        Ok(StoredEntryFixed::encode(addr_bytes, &packed)?)
    }

    /// Drops every cached unspent-output count.
    ///
    /// The cache is derived data, so this is the single consistency mechanism:
    /// call it whenever this process's in-memory view may have diverged from
    /// committed state — a failed write or delete (cache updates happen during
    /// the build phase, before the commit), or any evidence of another process
    /// writing the shared database. The only cost is re-amortization: the next
    /// spend of each affected transaction re-probes the committed spent index.
    pub(crate) fn invalidate_unspent_output_counts(&self) {
        self.unspent_output_counts.clear();
    }

    /// The operator-facing error for LMDB's `MapFull`: the configured database size
    /// cap was hit. The failing transaction aborted atomically, so the database is
    /// intact — raising `storage.database.size` and restarting resumes sync from
    /// the on-disk tip with nothing lost.
    fn map_full_config_error(&self) -> FinalisedStateError {
        FinalisedStateError::Custom(format!(
            "database hit the configured size cap (storage.database.size = {} GB): \
             raise it and restart; the database is intact and sync resumes from the \
             on-disk tip",
            self.config.storage.database.size.0
        ))
    }

    /// Persists one block's pre-built write data inside an open LMDB write transaction:
    /// every table put for the block, and nothing else — all entries arrive
    /// pre-encoded, so no serialization happens inside the single-writer window.
    /// Committing is the caller's responsibility, so several blocks can share one
    /// durable commit.
    fn put_block_write_data_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        data: BlockWriteData,
    ) -> Result<(), FinalisedStateError> {
        // Per-height entries: six tables share the height key and NO_OVERWRITE;
        // only the table and the pre-encoded entry differ.
        for (db, entry_bytes) in [
            (self.headers, &data.header_entry_bytes),
            (self.txids, &data.txid_entry_bytes),
            (self.transparent, &data.transparent_entry_bytes),
            (self.sapling, &data.sapling_entry_bytes),
            (self.orchard, &data.orchard_entry_bytes),
            (self.commitment_tree_data, &data.commitment_tree_entry_bytes),
        ] {
            txn.put(
                db,
                &data.block_height_bytes,
                entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
        }

        txn.put(
            self.heights,
            &data.block_hash_bytes,
            &data.height_entry_bytes,
            WriteFlags::NO_OVERWRITE,
        )?;

        // Reverse txid index: `txid -> TxLocation`.
        for (txid_bytes, entry_bytes) in &data.txid_location_entries {
            txn.put(
                self.txid_location,
                txid_bytes,
                entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
        }

        // Write spent to ZainoDB
        for (outpoint_bytes, entry_bytes) in &data.spent_entries {
            txn.put(
                self.spent,
                outpoint_bytes,
                entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
        }

        txn.put(
            self.tx_out_set_info_accumulator,
            &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
            &data.tx_out_set_info_accumulator_entry_bytes,
            WriteFlags::empty(),
        )?;

        #[cfg(feature = "transparent_address_history_experimental")]
        {
            // Write outputs to ZainoDB addrhist
            for (addr_script, records) in data.addrhist_outputs_map {
                let addr_bytes = addr_script.to_bytes()?;

                // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                let mut stored_entries = Vec::with_capacity(records.len());
                for record in records {
                    let entry_bytes = Self::encode_addr_hist_entry(&addr_bytes, &record)?;
                    stored_entries.push((record, entry_bytes));
                }

                // Order by byte encoding for LMDB DUP_SORT insertion order
                stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                for (_record, record_entry_bytes) in stored_entries {
                    txn.put(
                        self.address_history,
                        &addr_bytes,
                        &record_entry_bytes,
                        WriteFlags::empty(),
                    )?;
                }
            }

            // Write inputs to ZainoDB addrhist
            for (addr_script, records) in data.addrhist_inputs_map {
                let addr_bytes = addr_script.to_bytes()?;

                // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                let mut stored_entries = Vec::with_capacity(records.len());
                for (record, prev_output) in records {
                    let entry_bytes = Self::encode_addr_hist_entry(&addr_bytes, &record)?;
                    stored_entries.push((record, entry_bytes, prev_output));
                }

                // Order by byte encoding for LMDB DUP_SORT insertion order
                stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                for (_record, record_entry_bytes, (prev_output_script, prev_output_record)) in
                    stored_entries
                {
                    txn.put(
                        self.address_history,
                        &addr_bytes,
                        &record_entry_bytes,
                        WriteFlags::empty(),
                    )?;

                    // mark corresponding output as spent
                    let prev_addr_bytes = prev_output_script.to_bytes()?;
                    let prev_entry_bytes =
                        Self::encode_addr_hist_entry(&prev_addr_bytes, &prev_output_record)?;
                    let updated = self.mark_addr_hist_record_spent_in_txn(
                        &mut *txn,
                        &prev_output_script,
                        &prev_entry_bytes,
                    )?;
                    if !updated {
                        // Log and treat as invalid block — marking the prev-output must succeed.
                        return Err(FinalisedStateError::InvalidBlock {
                            height: data.block_height.0,
                            hash: data.block_hash,
                            reason: format!(
                                "failed to mark prev-output spent: addr={} tloc={:?} vout={}",
                                hex::encode(addr_bytes),
                                prev_output_record.tx_location(),
                                prev_output_record.out_index()
                            ),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Clone of `self` for a background task (write, validation/scan, or compact-block
    /// streaming): shares the LMDB env, table handles, and shared atomics, but starts
    /// with an empty `db_handler` slot, matching the long-standing behavior of every
    /// task clone.
    pub(super) fn task_clone(&self) -> Self {
        Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            spent: self.spent,
            txid_location: self.txid_location,
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            unspent_output_counts: Arc::clone(&self.unspent_output_counts),
            transparent_utxo_cache: self.transparent_utxo_cache.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        }
    }

    /// Writes a contiguous batch of finalised [`IndexedBlock`]s in a single LMDB write
    /// transaction — one durable commit (data + meta fsync) for the whole batch instead
    /// of one per block.
    ///
    /// ## Batch preconditions
    ///
    /// - Blocks are height-contiguous and the first block is `db_tip + 1` (both checked
    ///   here).
    ///
    /// Blocks may freely reference state written by earlier blocks of the same batch
    /// (spend their outputs, spend sibling outputs of transactions they spent from):
    /// the build phase threads a [`PendingBatchState`] overlay through the batch and
    /// consults it before the committed tables.
    ///
    /// ## Failure semantics
    ///
    /// The batch commits atomically: on any error nothing is persisted and there is
    /// nothing to roll back. This method has none of `write_block`'s idempotent-rewrite
    /// or multi-process recovery handling — callers should retry a failed batch
    /// block-by-block via [`DbV1::write_block`].
    ///
    /// ## Verification
    ///
    /// The write path does not validate. The background validator was removed; committed
    /// records are served without a read-back/re-hash pass.
    pub(crate) async fn write_blocks(
        &self,
        blocks: &[IndexedBlock],
    ) -> Result<(), FinalisedStateError> {
        let Some(first) = blocks.first() else {
            return Ok(());
        };
        self.status.store(StatusType::Syncing);

        for pair in blocks.windows(2) {
            let (prev, next) = (pair[0].context.index.height, pair[1].context.index.height);
            if next.0 != prev.0 + 1 {
                return Err(FinalisedStateError::Custom(format!(
                    "write_blocks batch is not height-contiguous: {} is followed by {}",
                    prev.0, next.0
                )));
            }
        }

        let first_height = first.context.index.height;
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.headers)?;
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((last_height_bytes, _)) => {
                    let last_height = Height::from_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?;
                    if first_height.0 != last_height.0 + 1 {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot write batch starting at height {first_height:?}; \
                             current tip is {last_height:?}"
                        )));
                    }
                }
                Err(lmdb::Error::NotFound) => {
                    if first_height.0 != GENESIS_HEIGHT.0 {
                        return Err(FinalisedStateError::Custom(format!(
                            "first block of a batch on an empty database must be height 0, \
                             got {first_height:?}"
                        )));
                    }
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }
            Ok::<_, FinalisedStateError>(())
        })?;

        // Build phase: reads resolve against committed state plus the in-memory overlay
        // of earlier batch blocks (accumulator, transactions, spends), so batch blocks
        // may freely spend outputs their batch created.
        let mut batch_data = Vec::with_capacity(blocks.len());
        let mut pending = PendingBatchState::new();
        for block in blocks {
            let data = self.build_block_write_data(block, Some(&pending)).await?;
            pending.absorb(block, &data);
            batch_data.push(data);
        }

        let zaino_db = self.task_clone();
        let join_handle = tokio::task::spawn_blocking(move || {
            let mut txn = zaino_db.env.begin_rw_txn()?;
            for data in batch_data {
                zaino_db.put_block_write_data_in_txn(&mut txn, data)?;
            }
            // One durable commit (data + meta fsync) for the whole batch. The write path does
            // not validate; the background validator was removed.
            txn.commit()?;
            Ok::<_, FinalisedStateError>(())
        });

        let post_result = match join_handle.await {
            Ok(inner) => inner,
            Err(join_err) => Err(FinalisedStateError::Custom(format!(
                "Tokio task error: {join_err}"
            ))),
        };

        match post_result {
            Ok(()) => {
                self.status.store(StatusType::Ready);
                info!(
                    "Committed batch of {} blocks ({}..={}) to ZainoDB.",
                    blocks.len(),
                    first_height.0,
                    first_height.0 + blocks.len() as u32 - 1,
                );
                Ok(())
            }
            Err(e) => {
                // Every failure here precedes a successful commit, and LMDB commits are
                // atomic, so nothing was persisted on disk — but the build phase already
                // applied this batch's unspent-count updates and UTXO cache in memory.
                self.invalidate_unspent_output_counts();
                self.reseed_transparent_utxo_cache()?;

                if matches!(e, FinalisedStateError::LmdbError(lmdb::Error::MapFull)) {
                    self.status.store(StatusType::RecoverableError);
                    return Err(self.map_full_config_error());
                }
                warn!(
                    "Batched block write failed ({e}); nothing persisted ({}..={})",
                    first_height.0,
                    first_height.0 + blocks.len() as u32 - 1,
                );
                self.status.store(StatusType::RecoverableError);
                Err(e)
            }
        }
    }

    /// Deletes a block identified height from every finalised table.
    pub(crate) async fn delete_block_at_height(
        &self,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        let result = self.delete_block_at_height_inner(height).await;
        if result.is_err() {
            // Cache updates happen during the delete's build phase, before its
            // commit; on failure the in-memory counts are ahead of disk.
            self.invalidate_unspent_output_counts();
        }
        result
    }

    async fn delete_block_at_height_inner(
        &self,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        // Check block is at the top of the finalised state
        tokio::task::block_in_place(|| {
            let height_bytes = height.to_bytes()?;
            let ro = self.env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(self.headers)?;

            let mut iter = cursor.iter_from(&height_bytes);

            let Some((current_height_bytes, _)) = iter.next() else {
                return Err(FinalisedStateError::Custom("block not found".into()));
            };
            if current_height_bytes != height_bytes.as_slice() {
                return Err(FinalisedStateError::Custom(format!(
                    "block with height {:?} not found in headers",
                    Height::from_bytes(&height_bytes)?
                )));
            }

            if iter.next().is_some() {
                return Err(FinalisedStateError::Custom(format!(
                    "can only delete tip block at height {:?}, but higher blocks exist",
                    Height::from_bytes(&height_bytes)?
                )));
            }
            Ok::<_, FinalisedStateError>(())
        })?;

        // fetch chain_block from db and delete
        let Some(chain_block) = self.get_chain_block(height).await? else {
            return Err(FinalisedStateError::DataUnavailable(format!(
                "attempted to delete missing block: {}",
                height.0
            )));
        };
        self.delete_block(&chain_block).await?;

        tokio::task::block_in_place(|| {
            self.env
                .sync(true)
                .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
            Ok::<_, FinalisedStateError>(())
        })?;

        Ok(())
    }

    /// This is used as a backup when delete_block_at_height fails.
    ///
    /// Takes a IndexedBlock as input and ensures all data from this block is wiped from the database.
    ///
    /// The IndexedBlock ir required to ensure that Outputs spent at this block height are re-marked as unspent.
    ///
    /// WARNING: No checks are made that this block is at the top of the finalised state, and validated tip is not updated.
    /// This enables use for correcting corrupt data within the database but it is left to the user to ensure safe use.
    /// Where possible delete_block_at_height should be used instead.
    ///
    /// NOTE: LMDB database errors are propageted as these show serious database errors,
    /// all other errors are returned as `IncorrectBlock`, if this error is returned the block requested
    /// should be fetched from the validator and this method called with the correct data.
    pub(crate) async fn delete_block(
        &self,
        block: &IndexedBlock,
    ) -> Result<(), FinalisedStateError> {
        let result = self.delete_block_inner(block).await;
        if result.is_err() {
            // Cache updates happen during the delete's build phase, before its
            // commit; on failure the in-memory counts are ahead of disk.
            self.invalidate_unspent_output_counts();
        }
        result
    }

    async fn delete_block_inner(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        // Check block height and hash
        let block_height = block.context.index.height;
        let block_height_bytes =
            block_height
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        let block_hash = block.context.index.hash;
        let block_hash_bytes =
            block_hash
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        // Build transaction indexes.
        //
        // See `write_block` for the rationale on pairing the txid and transparent data at
        // construction. Same source-pairing guarantee here.
        let tx_len = block.transactions().len();
        let mut transactions: Vec<(TransactionHash, Option<TransparentCompactTx>)> =
            Vec::with_capacity(tx_len);
        // txid -> in-block index. `insert` returning `Some` is the duplicate-txid
        // guard; the index also makes in-block prevout lookups O(1).
        let mut txid_index: HashMap<TransactionHash, u16> = HashMap::with_capacity(tx_len);

        #[cfg(feature = "transparent_address_history_experimental")]
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            // Bound the index to the narrow u16 form the dup map (and, under the
            // address-history feature, the tx location) require.
            let tx_index =
                u16::try_from(tx_index).map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("transaction index {tx_index} does not fit into u16"),
                })?;

            if txid_index.insert(*hash, tx_index).is_some() {
                return Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("duplicate transaction hash in block: {hash:?}"),
                });
            }

            // Transparent transactions — paired with the txid at the source binding.
            let transparent_data = stored_transparent_data(tx);
            transactions.push((*hash, transparent_data));

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                let tx_location = TxLocation::new(block_height.into(), tx_index);
                // Transparent Outputs: Build Address History
                DbV1::build_transaction_output_histories(
                    &mut addrhist_outputs_map,
                    tx_location,
                    tx.transparent().outputs().iter().enumerate(),
                );

                // Transparent Inputs: Build Address History
                for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                    if input.is_null_prevout() {
                        continue;
                    }

                    let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                    // Check if output is in *this* block, else fetch from DB.
                    let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                    if let Some(&prev_idx) = txid_index.get(&prev_tx_hash) {
                        // In-bounds by construction: `prev_idx` was assigned when that
                        // transaction was pushed into `transactions`, and the current
                        // transaction is pushed before its inputs are processed.
                        if let (_, Some(prev_transparent)) = &transactions[prev_idx as usize] {
                            // Fetch output from transaction
                            if let Some(prev_output) = prev_transparent
                                .outputs()
                                .get(prev_outpoint.prev_index() as usize)
                            {
                                let prev_output_tx_location =
                                    TxLocation::new(block_height.0, prev_idx);
                                DbV1::build_input_history(
                                    &mut addrhist_inputs_map,
                                    tx_location,
                                    input_index as u16,
                                    input,
                                    prev_output,
                                    prev_output_tx_location,
                                );
                            }
                        }
                    } else if let Ok((prev_output, prev_output_tx_location)) =
                        tokio::task::block_in_place(|| {
                            let prev_output = self.get_previous_output_blocking(prev_outpoint)?;

                            let prev_output_tx_location = self
                                .find_txid_index_blocking(&TransactionHash::from(
                                    *prev_outpoint.prev_txid(),
                                ))
                                .map_err(|e| FinalisedStateError::InvalidBlock {
                                    height: block.height().0,
                                    hash: *block.hash(),
                                    reason: e.to_string(),
                                })?
                                .ok_or_else(|| FinalisedStateError::InvalidBlock {
                                    height: block.height().0,
                                    hash: *block.hash(),
                                    reason: "Invalid block data: invalid txid data.".to_string(),
                                })?;

                            Ok::<(_, _), FinalisedStateError>((
                                prev_output,
                                prev_output_tx_location,
                            ))
                        })
                    {
                        DbV1::build_input_history(
                            &mut addrhist_inputs_map,
                            tx_location,
                            input_index as u16,
                            input,
                            &prev_output,
                            prev_output_tx_location,
                        );
                    } else {
                        return Err(FinalisedStateError::InvalidBlock {
                            height: block.height().0,
                            hash: *block.hash(),
                            reason: "Invalid block data: invalid transparent input.".to_string(),
                        });
                    }
                }
            }
        }

        // Same transparent delta the forward path uses, here driving the reverse
        // accumulator and the spent-index removal below.
        let transparent_delta =
            transparent_delta::block_transparent_delta(block_height, &transactions)?;
        let spent_map = transparent_delta::spent_map_from_delta(&transparent_delta);

        let tx_out_set_info_accumulator = self
            .calculate_tx_out_set_info_accumulator_after_delete_block(&transactions, &spent_map)
            .await?;

        // Reverse txid index keys written for this block by `write_block`.
        let txid_location_keys: Vec<[u8; 32]> = transactions
            .iter()
            .map(|(txid, _)| (*txid).into())
            .collect();

        // Delete all block data from db.
        let zaino_db = self.task_clone();
        tokio::task::spawn_blocking(move || {
            let mut txn = zaino_db.env.begin_rw_txn()?;

            let tx_out_set_info_accumulator_entry =
                StoredEntryFixed::new(TX_OUT_SET_INFO_ACCUMULATOR_KEY, tx_out_set_info_accumulator);

            txn.put(
                zaino_db.tx_out_set_info_accumulator,
                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                &tx_out_set_info_accumulator_entry.to_bytes()?,
                WriteFlags::empty(),
            )?;

            // Delete spent data
            for outpoint in spent_map.keys() {
                let outpoint_bytes =
                    &outpoint
                        .to_bytes()
                        .map_err(|_| FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: "Corrupt block data: failed to serialise outpoint".to_string(),
                        })?;

                match txn.del(zaino_db.spent, outpoint_bytes, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            }

            // Delete reverse txid index data.
            for txid_bytes in &txid_location_keys {
                match txn.del(zaino_db.txid_location, txid_bytes, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            }

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Delete addrhist input data and mark old outputs spent in this block as unspent
                for (addr_script, records) in &addrhist_inputs_map {
                    let addr_bytes = addr_script.to_bytes()?;

                    // Mark outputs spent in this block as unspent
                    for (_record, (prev_output_script, prev_output_record)) in records {
                        {
                            let prev_addr_bytes = prev_output_script.to_bytes()?;

                            // Build the *spent* form of the stored entry so it matches the DB
                            // (mark_addr_hist_record_spent_blocking sets FLAG_SPENT and
                            // recomputes the checksum).  We must pass the spent bytes here
                            // because the DB currently contains the spent version.
                            let prev_entry_bytes =
                                Self::encode_addr_hist_entry(&prev_addr_bytes, prev_output_record)?;

                            // Turn the mined-entry into the spent-entry (mutate flags + checksum)
                            let mut spent_prev_entry = prev_entry_bytes.clone();
                            // Set SPENT flag (flags byte is at index 10 in StoredEntry layout)
                            spent_prev_entry[10] |= AddrHistRecord::FLAG_SPENT;
                            // Recompute checksum over bytes 1..19 as StoredEntryFixed expects.
                            let checksum =
                                keyed_checksum(&prev_addr_bytes, &spent_prev_entry[1..19]);
                            spent_prev_entry[19..51].copy_from_slice(&checksum);

                            let updated = zaino_db.mark_addr_hist_record_unspent_in_txn(
                                &mut txn,
                                prev_output_script,
                                &spent_prev_entry,
                            )?;

                            if !updated {
                                // Log and treat as invalid block — marking the prev-output must succeed.
                                return Err(FinalisedStateError::InvalidBlock {
                                    height: block_height.0,
                                    hash: block_hash,
                                    reason: format!(
                                    "failed to mark prev-output spent: addr={} tloc={:?} vout={}",
                                    hex::encode(addr_bytes),
                                    prev_output_record.tx_location(),
                                    prev_output_record.out_index()
                                ),
                                });
                            }
                        }
                    }

                    // Delete all input records created in this block.
                    zaino_db
                        .delete_addrhist_dups_in_txn(
                            &mut txn,
                            &addr_script.to_bytes().map_err(|_| {
                                FinalisedStateError::InvalidBlock {
                                    height: block_height.0,
                                    hash: block_hash,
                                    reason: "Corrupt block data: failed to serialise addr_script"
                                        .to_string(),
                                }
                            })?,
                            block_height,
                            true,
                            false,
                            records.len(),
                        )
                        // TODO: check internals to propagate important errors.
                        .map_err(|_| FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: "Corrupt block data: failed to delete inputs".to_string(),
                        })?;
                }

                // Delete addrhist output data
                for (addr_script, records) in &addrhist_outputs_map {
                    zaino_db.delete_addrhist_dups_in_txn(
                        &mut txn,
                        &addr_script
                            .to_bytes()
                            .map_err(|_| FinalisedStateError::InvalidBlock {
                                height: block_height.0,
                                hash: block_hash,
                                reason: "Corrupt block data: failed to serialise addr_script"
                                    .to_string(),
                            })?,
                        block_height,
                        false,
                        true,
                        records.len(),
                    )?;
                }
            }

            // Delete block data
            for &db in &[
                zaino_db.headers,
                zaino_db.txids,
                zaino_db.transparent,
                zaino_db.sapling,
                zaino_db.orchard,
                zaino_db.commitment_tree_data,
            ] {
                match txn.del(db, &block_height_bytes, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            }

            match txn.del(zaino_db.heights, &block_hash_bytes, None) {
                Ok(()) | Err(lmdb::Error::NotFound) => {}
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }

            let _ = txn.commit();

            zaino_db
                .env
                .sync(true)
                .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;

            Ok::<_, FinalisedStateError>(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("Tokio task error: {e}")))??;

        // The block is gone; reseed the in-memory UTXO cache from the post-delete
        // committed state. delete is the rare finalised rollback/correction path —
        // reorgs never enter the finalised state (the NFS owns those) — so a full
        // reseed is cheaper and simpler than threading spent-output values through a
        // precise inverse. (`block_in_place` inside the seed is valid here, in the
        // async context, but would not be inside the blocking delete closure above.)
        self.reseed_transparent_utxo_cache()?;

        Ok(())
    }

    /// Updates the metadata hed by the database.
    pub(crate) async fn update_metadata(
        &self,
        metadata: DbMetadata,
    ) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            let entry = StoredEntryFixed::new(b"metadata", metadata);
            txn.put(
                self.metadata,
                b"metadata",
                &entry.to_bytes()?,
                WriteFlags::empty(),
            )?;

            txn.commit()?;
            Ok(())
        })
    }
}

#[cfg(test)]
impl DbV1 {
    /// Writes a block using the v1.0.0 format.
    ///
    /// This intentionally writes only the core v1 tables and uses v1 item encodings.
    ///
    /// This method does not perform safety checks and must not be used in production code.
    ///
    /// Used for migration tests.
    pub(crate) async fn write_block_v1_0_0(
        &self,
        block: IndexedBlock,
    ) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);

        let block_hash = block.context.index.hash;
        let block_hash_bytes = block_hash.to_bytes()?;
        let block_height = block.context.index.height;
        let block_height_bytes = block_height.to_bytes()?;

        let height_entry_bytes = StoredEntryFixed::<Height>::to_bytes_with_item_version(
            &block_hash_bytes,
            &block.context.index.height,
            version::V1,
        )?;

        let header = BlockHeaderData::new(block.context, *block.data());
        let header_entry_bytes = StoredEntryVar::<BlockHeaderData>::to_bytes_with_item_version(
            &block_height_bytes,
            &header,
            version::V1,
        )?;

        let commitment_tree_entry_bytes =
            StoredEntryFixed::<CommitmentTreeData>::to_bytes_with_item_version(
                &block_height_bytes,
                block.commitment_tree_data(),
                version::V1,
            )?;

        let tx_len = block.transactions().len();
        let mut txids = Vec::with_capacity(tx_len);
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut transparent = Vec::with_capacity(tx_len);
        let mut sapling = Vec::with_capacity(tx_len);
        let mut orchard = Vec::with_capacity(tx_len);

        for tx in block.transactions() {
            let hash = tx.txid();

            if txid_set.insert(*hash) {
                txids.push(*hash);
            }

            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transparent.push(transparent_data);

            let sapling_data = stored_sapling_data(tx);
            sapling.push(sapling_data);

            let orchard_data = stored_orchard_data(tx);
            orchard.push(orchard_data);
        }

        let txid_list = TxidList::new(txids);
        let txid_entry_bytes = StoredEntryVar::<TxidList>::to_bytes_with_item_version(
            &block_height_bytes,
            &txid_list,
            version::V1,
        )?;

        let transparent_tx_list = TransparentTxList::new(transparent);
        let transparent_entry_bytes =
            StoredEntryVar::<TransparentTxList>::to_bytes_with_item_version(
                &block_height_bytes,
                &transparent_tx_list,
                version::V1,
            )?;

        let sapling_tx_list = SaplingTxList::new(sapling);
        let sapling_entry_bytes = StoredEntryVar::<SaplingTxList>::to_bytes_with_item_version(
            &block_height_bytes,
            &sapling_tx_list,
            version::V1,
        )?;

        let orchard_tx_list = OrchardTxList::new(orchard);
        let orchard_entry_bytes = StoredEntryVar::<OrchardTxList>::to_bytes_with_item_version(
            &block_height_bytes,
            &orchard_tx_list,
            version::V1,
        )?;

        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            txn.put(
                self.headers,
                &block_height_bytes,
                &header_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.heights,
                &block_hash_bytes,
                &height_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.txids,
                &block_height_bytes,
                &txid_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.transparent,
                &block_height_bytes,
                &transparent_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.sapling,
                &block_height_bytes,
                &sapling_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.orchard,
                &block_height_bytes,
                &orchard_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
            txn.put(
                self.commitment_tree_data,
                &block_height_bytes,
                &commitment_tree_entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.commit()?;
            self.env.sync(true)?;

            Ok::<_, FinalisedStateError>(())
        })?;

        self.status.store(StatusType::Ready);
        Ok(())
    }
}

/// Pre-built write data for one block: encoded key bytes, checksummed table entries,
/// per-block index maps, and the post-block txout-set accumulator. Produced by
/// [`DbV1::build_block_write_data`] and consumed inside an LMDB write transaction by
/// [`DbV1::put_block_write_data_in_txn`].
struct BlockWriteData {
    block_hash: BlockHash,
    block_height: Height,
    block_hash_bytes: Vec<u8>,
    block_height_bytes: Vec<u8>,
    height_entry_bytes: Vec<u8>,
    header_entry_bytes: Vec<u8>,
    commitment_tree_entry_bytes: Vec<u8>,
    /// `(txid, encoded entry)`, sorted by txid so the random-keyed `txid_location`
    /// B-tree sees locally-ordered inserts.
    txid_location_entries: Vec<([u8; 32], Vec<u8>)>,
    txid_entry_bytes: Vec<u8>,
    transparent_entry_bytes: Vec<u8>,
    sapling_entry_bytes: Vec<u8>,
    orchard_entry_bytes: Vec<u8>,
    /// `(encoded outpoint key, encoded entry)` for the `spent` table.
    spent_entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Kept alongside `spent_entries`: `PendingBatchState::absorb` and the
    /// accumulator calculation consume the typed map.
    spent_map: HashMap<Outpoint, TxLocation>,
    tx_out_set_info_accumulator: FinalisedTxOutSetInfoAccumulator,
    tx_out_set_info_accumulator_entry_bytes: Vec<u8>,
    #[cfg(feature = "transparent_address_history_experimental")]
    addrhist_inputs_map: HashMap<AddrScript, Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>>,
    #[cfg(feature = "transparent_address_history_experimental")]
    addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>>,
}

/// In-memory overlay of everything an open write batch has produced but not yet
/// committed: the txout-set accumulator after the latest pending block, every
/// pending transaction (with its location and transparent data), and every
/// outpoint the batch spends. Build-phase reads consult this before the
/// committed tables, so a batch block may spend outputs created — or sibling
/// outputs of transactions spent from — earlier in the same batch.
// pub(crate) (not pub(super)) because it appears in the signature of the
// pub(crate) accumulator calculation that migrations.rs also calls.
pub(crate) struct PendingBatchState {
    /// Txout-set accumulator after the latest pending block; `None` until the
    /// first block of the batch is built.
    pub(super) accumulator: Option<FinalisedTxOutSetInfoAccumulator>,
    /// txid -> (location, transparent data) for every pending transaction.
    pub(super) transactions: HashMap<TransactionHash, (TxLocation, Option<TransparentCompactTx>)>,
    /// Outpoints spent by pending blocks, keyed to their spender's location.
    pub(super) spent: HashMap<Outpoint, TxLocation>,
}

impl PendingBatchState {
    fn new() -> Self {
        Self {
            accumulator: None,
            transactions: HashMap::new(),
            spent: HashMap::new(),
        }
    }

    /// Absorbs a just-built block's contributions so later batch blocks can
    /// read them.
    fn absorb(&mut self, block: &IndexedBlock, data: &BlockWriteData) {
        self.accumulator = Some(data.tx_out_set_info_accumulator);
        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let tx_index = u16::try_from(tx_index)
                .expect("transaction index bounded by build_block_write_data");
            let location = TxLocation::new(block.context.index.height.0, tx_index);
            self.transactions
                .insert(*tx.txid(), (location, stored_transparent_data(tx)));
        }
        for (outpoint, location) in &data.spent_map {
            self.spent.insert(*outpoint, *location);
        }
    }
}

/// The stored form of a transaction's transparent data: `None` when the
/// transaction has no transparent inputs or outputs.
fn stored_transparent_data(tx: &CompactTxData) -> Option<TransparentCompactTx> {
    if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
        None
    } else {
        Some(tx.transparent().clone())
    }
}

/// The stored form of a transaction's sapling data: `None` when the
/// transaction has no sapling spends or outputs.
fn stored_sapling_data(tx: &CompactTxData) -> Option<SaplingCompactTx> {
    if tx.sapling().spends().is_empty() && tx.sapling().outputs().is_empty() {
        None
    } else {
        Some(tx.sapling().clone())
    }
}

/// The stored form of a transaction's orchard data: `None` when the
/// transaction has no orchard actions.
fn stored_orchard_data(tx: &CompactTxData) -> Option<OrchardCompactTx> {
    if tx.orchard().actions().is_empty() {
        None
    } else {
        Some(tx.orchard().clone())
    }
}
