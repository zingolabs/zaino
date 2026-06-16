//! ZainoDB::V1 core write functionality.

use super::*;
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

    async fn delete_block_at_height(&self, _height: Height) -> Result<(), FinalisedStateError> {
        Err(Self::delete_unsupported())
    }

    async fn delete_block(&self, _block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        Err(Self::delete_unsupported())
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
            return Ok(());
        }

        let data = self.build_block_write_data(&block, None).await?;

        // if any database writes fail, remove block from database and return err.
        let zaino_db = self.task_clone();
        let join_handle = tokio::task::spawn_blocking(move || {
            // Write block to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            zaino_db.put_block_write_data_in_txn(&mut txn, data)?;

            // `txn.commit()` fsyncs the data pages; the meta-page fsync is deferred under
            // NO_META_SYNC (one fsync per commit, not two). The data fsync still orders
            // data-before-meta, so a crash stays consistent and loses at most the last commit.
            //
            // The write path does not validate: the background validator was removed, so
            // committed records are served without a read-back/re-hash pass.
            txn.commit()?;

            Ok::<_, FinalisedStateError>(())
        });

        // Wait for the join and handle panic / cancellation explicitly. The write is a
        // single atomic LMDB commit, so a failed or cancelled task persisted nothing on
        // disk; recovery is to reset the in-memory derived state to committed (the same
        // reseed-from-committed primitive a restart runs) and surface the error. The
        // finalised index is append-only or restored-from-checkpoint, never rolled back
        // in place (docs/decision_records/finalised_state/append_only_design.md).
        let post_result = match join_handle.await {
            Ok(inner_res) => inner_res,
            Err(join_err) => {
                warn!("Tokio task error (spawn_blocking join error): {}", join_err);

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
                        self.status.store(StatusType::Ready);
                        info!(
                            "Block {} at height {} was already written by another process, skipping.",
                            &block_hash, &block_height.0
                        );
                        Ok(())
                    }
                    Err(e) => {
                        warn!("Error writing block to DB: {e}");

                        // Our atomic commit was rejected (a different block occupies this
                        // height), so nothing of ours reached disk; the committed block is
                        // never deleted in place.
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
                if matches!(e, FinalisedStateError::LmdbError(lmdb::Error::MapFull)) {
                    // The transaction aborted atomically: nothing was committed and the
                    // database is intact (only the size cap was reached).
                    self.status.store(StatusType::RecoverableError);
                    return Err(self.map_full_config_error());
                }

                warn!("Error writing block to DB: {e}");

                // The commit aborted atomically: nothing was persisted. The append-only
                // index is never rolled back in place.
                self.status.store(StatusType::RecoverableError);

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
    /// the experimental address-history path's prevout resolution consults it before the
    /// committed tables, so a batch block may reference outputs an earlier, uncommitted
    /// batch block created. Pass `None` on the single-block write path, where all prior
    /// state is committed. The non-experimental build resolves nothing from the overlay
    /// (the spent index needs no prevout data), so `pending` is unused there.
    #[cfg_attr(
        not(feature = "transparent_address_history_experimental"),
        allow(unused_variables)
    )]
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

        // Derive the block's transparent delta once; the spent index consumes it
        // instead of re-walking the transactions. The txout-set accumulator is no
        // longer maintained here — it is rebuilt lazily from the committed tables on
        // first query (see `get_tx_out_set_info_accumulator`).
        let transparent_delta =
            transparent_delta::block_transparent_delta(block_height, &transactions)?;
        let spent_map = transparent_delta::spent_map_from_delta(&transparent_delta);

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

        // Pre-encode the spent-index entries too, so the write transaction performs
        // no serialization at all.
        let mut spent_entries = Vec::with_capacity(spent_map.len());
        for (outpoint, tx_location) in &spent_map {
            let outpoint_bytes = outpoint.to_bytes()?;
            let entry_bytes = StoredEntryFixed::encode(&outpoint_bytes, tx_location)?;
            spent_entries.push((outpoint_bytes, entry_bytes));
        }

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
    /// Writes the six height-keyed tables (headers/txids/transparent/sapling/orchard/
    /// commitment tree) plus the hash→height reverse entry for one block. These keys are
    /// height-sequential, so their insertion order is already optimal.
    fn put_block_height_keyed_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        data: &BlockWriteData,
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
        Ok(())
    }

    /// Writes a whole batch of blocks in one transaction with the random-keyed `spent` and
    /// `txid_location` entries inserted in **sorted key order across the entire batch**.
    ///
    /// Those two indexes otherwise fault a scattered B-tree leaf per insert once the DB
    /// exceeds RAM; collecting every batch entry, sorting by key, and inserting in order
    /// turns that into a sequential B-tree sweep. Height-keyed tables are written per block
    /// (already sequential). The txout-set accumulator is not written here — it is rebuilt
    /// lazily from the committed tables on first query. Address-history is not batchable (its
    /// prev-output resolution depends on earlier-in-batch writes), so the experimental
    /// feature keeps the per-block path in `write_blocks`.
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    fn put_block_batch_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        batch: Vec<BlockWriteData>,
    ) -> Result<(), FinalisedStateError> {
        let mut txid_location: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        let mut spent: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        for mut data in batch {
            self.put_block_height_keyed_in_txn(txn, &data)?;
            txid_location.append(&mut data.txid_location_entries);
            spent.append(&mut data.spent_entries);
        }

        // Sorted sweep over each random-keyed B-tree. LMDB's default comparator is bytewise,
        // so byte-ascending key order is the B-tree's physical order.
        txid_location.sort_by(|a, b| a.0.cmp(&b.0));
        for (txid_bytes, entry_bytes) in &txid_location {
            txn.put(
                self.txid_location,
                txid_bytes,
                entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
        }
        spent.sort_by(|a, b| a.0.cmp(&b.0));
        for (outpoint_bytes, entry_bytes) in &spent {
            txn.put(
                self.spent,
                outpoint_bytes,
                entry_bytes,
                WriteFlags::NO_OVERWRITE,
            )?;
        }

        Ok(())
    }

    fn put_block_write_data_in_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        data: BlockWriteData,
    ) -> Result<(), FinalisedStateError> {
        self.put_block_height_keyed_in_txn(txn, &data)?;

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
            accumulator_rebuild_lock: Arc::clone(&self.accumulator_rebuild_lock),
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
            // Batch-wide sorted inserts for the random-keyed indexes (sequential B-tree
            // sweep). The address-history feature can't be batched, so it stays per block.
            #[cfg(not(feature = "transparent_address_history_experimental"))]
            zaino_db.put_block_batch_in_txn(&mut txn, batch_data)?;
            #[cfg(feature = "transparent_address_history_experimental")]
            for data in batch_data {
                zaino_db.put_block_write_data_in_txn(&mut txn, data)?;
            }
            // One durable commit for the whole batch (data fsync; the meta-page fsync is
            // deferred under NO_META_SYNC). The write path does not validate; the
            // background validator was removed.
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
                // atomic, so nothing was persisted on disk.
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

    /// The append-only V1 backend does not support deleting committed blocks; reset is
    /// a checkpoint-restore or a fresh sync. See
    /// docs/decision_records/finalised_state/append_only_design.md.
    fn delete_unsupported() -> FinalisedStateError {
        FinalisedStateError::Custom(
            "the append-only V1 finalised backend does not support block deletion; \
             reset is checkpoint-restore or a fresh sync"
                .to_string(),
        )
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
    /// Kept alongside `spent_entries`: `PendingBatchState::absorb` consumes the
    /// typed map.
    spent_map: HashMap<Outpoint, TxLocation>,
    #[cfg(feature = "transparent_address_history_experimental")]
    addrhist_inputs_map: HashMap<AddrScript, Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>>,
    #[cfg(feature = "transparent_address_history_experimental")]
    addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>>,
}

/// In-memory overlay of everything an open write batch has produced but not yet
/// committed: every pending transaction (with its location and transparent data),
/// and every outpoint the batch spends. Build-phase reads consult this before the
/// committed tables, so a batch block may spend outputs created — or sibling
/// outputs of transactions spent from — earlier in the same batch.
pub(crate) struct PendingBatchState {
    /// txid -> (location, transparent data) for every pending transaction.
    pub(super) transactions: HashMap<TransactionHash, (TxLocation, Option<TransparentCompactTx>)>,
    /// Outpoints spent by pending blocks, keyed to their spender's location.
    pub(super) spent: HashMap<Outpoint, TxLocation>,
}

impl PendingBatchState {
    fn new() -> Self {
        Self {
            transactions: HashMap::new(),
            spent: HashMap::new(),
        }
    }

    /// Absorbs a just-built block's contributions so later batch blocks can
    /// read them.
    fn absorb(&mut self, block: &IndexedBlock, data: &BlockWriteData) {
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
