//! ZainoDB::V1 core write functionality.

use super::*;

/// [`DbWrite`] capability implementation for [`DbV1`].
///
/// This trait represents the mutating surface (append / delete tip / update metadata). Writes are
/// performed via LMDB write transactions and validated before becoming visible as “known-good”.
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
    pub(crate) async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);
        let block_hash = *block.index().hash();
        let block_hash_bytes = block_hash.to_bytes()?;
        let block_height = block.index().height();
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
                    if *stored_header.index().hash() == block_hash {
                        // Same block already written, this is a no-op success
                        return Ok(true);
                    } else {
                        return Err(FinalisedStateError::Custom(format!(
                            "block at height {block_height:?} already exists with different hash \
                             (stored: {:?}, incoming: {:?})",
                            stored_header.index().hash(),
                            block_hash
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

        // Build DBHeight
        let height_entry = StoredEntryFixed::new(&block_hash_bytes, block.index().height());

        // Build header
        let header_entry = StoredEntryVar::new(
            &block_height_bytes,
            BlockHeaderData::new(*block.index(), *block.data()),
        );

        // Build commitment tree data
        let commitment_tree_entry =
            StoredEntryFixed::new(&block_height_bytes, *block.commitment_tree_data());

        // Build transaction indexes
        let tx_len = block.transactions().len();
        let mut txids = Vec::with_capacity(tx_len);
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut transparent = Vec::with_capacity(tx_len);
        let mut sapling = Vec::with_capacity(tx_len);
        let mut orchard = Vec::with_capacity(tx_len);

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut spent_map: HashMap<Outpoint, TxLocation> = HashMap::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        #[allow(clippy::unused_enumerate_index)]
        for (_tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            if txid_set.insert(*hash) {
                txids.push(*hash);
            }

            // Transparent transactions
            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transparent.push(transparent_data);

            // Sapling transactions
            let sapling_data =
                if tx.sapling().spends().is_empty() && tx.sapling().outputs().is_empty() {
                    None
                } else {
                    Some(tx.sapling().clone())
                };
            sapling.push(sapling_data);

            // Orchard transactions
            let orchard_data = if tx.orchard().actions().is_empty() {
                None
            } else {
                Some(tx.orchard().clone())
            };
            orchard.push(orchard_data);

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Transaction location
                let tx_location = TxLocation::new(block_height.into(), _tx_index as u16);

                // Transparent Outputs: Build Address History
                DbV1::build_transaction_output_histories(
                    &mut addrhist_outputs_map,
                    tx_location,
                    tx.transparent().outputs().iter().enumerate(),
                );

                // Transparent Inputs: Build Spent Outpoints Index and Address History
                for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                    if input.is_null_prevout() {
                        continue;
                    }
                    let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                    spent_map.insert(prev_outpoint, tx_location);

                    //Check if output is in *this* block, else fetch from DB.
                    let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                    if txid_set.contains(&prev_tx_hash) {
                        // Fetch transaction index within block
                        if let Some(tx_index) = txids.iter().position(|h| h == &prev_tx_hash) {
                            // Fetch Transparent data for transaction
                            if let Some(Some(prev_transparent)) = transparent.get(tx_index) {
                                // Fetch output from transaction
                                if let Some(prev_output) = prev_transparent
                                    .outputs()
                                    .get(prev_outpoint.prev_index() as usize)
                                {
                                    let prev_output_tx_location =
                                        TxLocation::new(block_height.0, tx_index as u16);
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

        let txid_entry = StoredEntryVar::new(&block_height_bytes, TxidList::new(txids));
        let transparent_entry =
            StoredEntryVar::new(&block_height_bytes, TransparentTxList::new(transparent));
        let sapling_entry = StoredEntryVar::new(&block_height_bytes, SaplingTxList::new(sapling));
        let orchard_entry = StoredEntryVar::new(&block_height_bytes, OrchardTxList::new(orchard));

        // if any database writes fail, or block validation fails, remove block from database and return err.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            #[cfg(feature = "transparent_address_history_experimental")]
            spent: self.spent,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: std::sync::Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        let join_handle = tokio::task::spawn_blocking(move || {
            // Write block to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            txn.put(
                zaino_db.headers,
                &block_height_bytes,
                &header_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.heights,
                &block_hash_bytes,
                &height_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.txids,
                &block_height_bytes,
                &txid_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.transparent,
                &block_height_bytes,
                &transparent_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.sapling,
                &block_height_bytes,
                &sapling_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.orchard,
                &block_height_bytes,
                &orchard_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.commitment_tree_data,
                &block_height_bytes,
                &commitment_tree_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Write spent to ZainoDB
                for (outpoint, tx_location) in spent_map {
                    let outpoint_bytes = &outpoint.to_bytes()?;
                    let tx_location_entry_bytes =
                        StoredEntryFixed::new(outpoint_bytes, tx_location).to_bytes()?;
                    txn.put(
                        zaino_db.spent,
                        &outpoint_bytes,
                        &tx_location_entry_bytes,
                        WriteFlags::NO_OVERWRITE,
                    )?;
                }

                // Write outputs to ZainoDB addrhist
                for (addr_script, records) in addrhist_outputs_map {
                    let addr_bytes = addr_script.to_bytes()?;

                    // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                    let mut stored_entries = Vec::with_capacity(records.len());
                    for record in records {
                        let packed_record = AddrEventBytes::from_record(&record).map_err(|e| {
                            FinalisedStateError::Custom(format!("AddrEventBytes pack error: {e:?}"))
                        })?;
                        let entry = StoredEntryFixed::new(&addr_bytes, packed_record);
                        let entry_bytes = entry.to_bytes()?;
                        stored_entries.push((record, entry_bytes));
                    }

                    // Order by byte encoding for LMDB DUP_SORT insertion order
                    stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                    for (_record, record_entry_bytes) in stored_entries {
                        txn.put(
                            zaino_db.address_history,
                            &addr_bytes,
                            &record_entry_bytes,
                            WriteFlags::empty(),
                        )?;
                    }
                }

                // Write inputs to ZainoDB addrhist
                for (addr_script, records) in addrhist_inputs_map {
                    let addr_bytes = addr_script.to_bytes()?;

                    // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                    let mut stored_entries = Vec::with_capacity(records.len());
                    for (record, prev_output) in records {
                        let packed_record = AddrEventBytes::from_record(&record).map_err(|e| {
                            FinalisedStateError::Custom(format!("AddrEventBytes pack error: {e:?}"))
                        })?;
                        let entry = StoredEntryFixed::new(&addr_bytes, packed_record);
                        let entry_bytes = entry.to_bytes()?;
                        stored_entries.push((record, entry_bytes, prev_output));
                    }

                    // Order by byte encoding for LMDB DUP_SORT insertion order
                    stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                    for (_record, record_entry_bytes, (prev_output_script, prev_output_record)) in
                        stored_entries
                    {
                        txn.put(
                            zaino_db.address_history,
                            &addr_bytes,
                            &record_entry_bytes,
                            WriteFlags::empty(),
                        )?;

                        // mark corresponding output as spent
                        let prev_addr_bytes = prev_output_script.to_bytes()?;
                        let packed_prev = AddrEventBytes::from_record(&prev_output_record)
                            .map_err(|e| {
                                FinalisedStateError::Custom(format!(
                                    "AddrEventBytes pack error: {e:?}"
                                ))
                            })?;
                        let prev_entry_bytes =
                            StoredEntryFixed::new(&prev_addr_bytes, packed_prev).to_bytes()?;
                        let updated = zaino_db.mark_addr_hist_record_spent_in_txn(
                            &mut txn,
                            &prev_output_script,
                            &prev_entry_bytes,
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
            }

            txn.commit()?;

            zaino_db.env.sync(true).map_err(|e| {
                FinalisedStateError::Custom(format!("LMDB sync failed before validation: {e}"))
            })?;

            zaino_db.validate_block_blocking(block_height, block_hash)?;

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

                return Err(FinalisedStateError::Custom(format!(
                    "Tokio task error: {}",
                    join_err
                )));
            }
        };

        match post_result {
            Ok(_) => {
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
                self.status.store(StatusType::Ready);
                if block.index().height().0.is_multiple_of(100) {
                    info!(
                        "Successfully committed block {} at height {} to ZainoDB.",
                        &block.index().hash(),
                        &block.index().height()
                    );
                } else {
                    tracing::debug!(
                        "Successfully committed block {} at height {} to ZainoDB.",
                        &block.index().hash(),
                        &block.index().height()
                    );
                }

                Ok(())
            }
            Err(FinalisedStateError::LmdbError(lmdb::Error::KeyExist)) => {
                // Block write failed because key already exists - another process wrote it
                // between our check and our write.
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
                            if *stored_header.index().hash() == block_hash {
                                // Block hash exists, verify block was fully written.
                                self.validate_block_blocking(block_height, block_hash)
                                    .map(|()| true)
                                    .map_err(|e| {
                                        FinalisedStateError::Custom(format!(
                                            "Block write fail at height {}, with hash {:?}, \
                                            validation error: {}",
                                            block_height.0, block_hash, e
                                        ))
                                    })
                            } else {
                                Err(FinalisedStateError::Custom(format!(
                                    "KeyExist race: different block at height {} \
                                     (stored: {:?}, incoming: {:?})",
                                    block_height.0,
                                    stored_header.index().hash(),
                                    block_hash
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
                        // Block was already written correctly by another process
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

                if e.to_string().contains("MDB_MAP_FULL") {
                    warn!("Configured max database size exceeded, update `storage.database.size` in zaino's config.");
                    return Err(FinalisedStateError::Custom(format!(
                        "Database configuration error: {e}"
                    )));
                }

                Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Deletes a block identified height from every finalised table.
    pub(crate) async fn delete_block_at_height(
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

        // update validated_tip / validated_set
        let validated_tip = self.validated_tip.load(Ordering::Acquire);
        if height.0 > validated_tip {
            self.validated_set.remove(&height.0);
        } else if height.0 == validated_tip {
            self.validated_tip
                .store(validated_tip.saturating_sub(1), Ordering::Release);
        }

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
        // Check block height and hash
        let block_height = block.index().height();
        let block_height_bytes =
            block_height
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        let block_hash = *block.index().hash();
        let block_hash_bytes =
            block_hash
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        // Build transaction indexes
        let tx_len = block.transactions().len();
        let mut txids = Vec::with_capacity(tx_len);
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut transparent = Vec::with_capacity(tx_len);

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut spent_map: Vec<Outpoint> = Vec::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();

        #[cfg(feature = "transparent_address_history_experimental")]
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        #[allow(clippy::unused_enumerate_index)]
        for (_tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            if txid_set.insert(*hash) {
                txids.push(*hash);
            }

            // Transparent transactions
            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transparent.push(transparent_data);

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Transaction location
                let tx_location = TxLocation::new(block_height.into(), _tx_index as u16);

                // Transparent Outputs: Build Address History
                DbV1::build_transaction_output_histories(
                    &mut addrhist_outputs_map,
                    tx_location,
                    tx.transparent().outputs().iter().enumerate(),
                );

                // Transparent Inputs: Build Spent Outpoints Index and Address History
                for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                    if input.is_null_prevout() {
                        continue;
                    }
                    let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                    spent_map.push(prev_outpoint);

                    //Check if output is in *this* block, else fetch from DB.
                    let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                    if txid_set.contains(&prev_tx_hash) {
                        // Fetch transaction index within block
                        if let Some(tx_index) = txids.iter().position(|h| h == &prev_tx_hash) {
                            // Fetch Transparent data for transaction
                            if let Some(Some(prev_transparent)) = transparent.get(tx_index) {
                                // Fetch output from transaction
                                if let Some(prev_output) = prev_transparent
                                    .outputs()
                                    .get(prev_outpoint.prev_index() as usize)
                                {
                                    let prev_output_tx_location =
                                        TxLocation::new(block_height.0, tx_index as u16);
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

        // Delete all block data from db.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            #[cfg(feature = "transparent_address_history_experimental")]
            spent: self.spent,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: std::sync::Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        tokio::task::spawn_blocking(move || {
            // Delete spent data
            let mut txn = zaino_db.env.begin_rw_txn()?;

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                for outpoint in &spent_map {
                    let outpoint_bytes =
                        &outpoint
                            .to_bytes()
                            .map_err(|_| FinalisedStateError::InvalidBlock {
                                height: block_height.0,
                                hash: block_hash,
                                reason: "Corrupt block data: failed to serialise outpoint"
                                    .to_string(),
                            })?;
                    match txn.del(zaino_db.spent, outpoint_bytes, None) {
                        Ok(()) | Err(lmdb::Error::NotFound) => {}
                        Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                    }
                }

                // Delete addrhist input data and mark old outputs spent in this block as unspent
                for (addr_script, records) in &addrhist_inputs_map {
                    let addr_bytes = addr_script.to_bytes()?;

                    // Mark outputs spent in this block as unspent
                    for (_record, (prev_output_script, prev_output_record)) in records {
                        {
                            let prev_addr_bytes = prev_output_script.to_bytes()?;
                            let packed_prev = AddrEventBytes::from_record(prev_output_record)
                                .map_err(|e| {
                                    FinalisedStateError::Custom(format!(
                                        "AddrEventBytes pack error: {e:?}"
                                    ))
                                })?;

                            // Build the *spent* form of the stored entry so it matches the DB
                            // (mark_addr_hist_record_spent_blocking sets FLAG_SPENT and
                            // recomputes the checksum).  We must pass the spent bytes here
                            // because the DB currently contains the spent version.
                            let prev_entry_bytes =
                                StoredEntryFixed::new(&prev_addr_bytes, packed_prev).to_bytes()?;

                            // Turn the mined-entry into the spent-entry (mutate flags + checksum)
                            let mut spent_prev_entry = prev_entry_bytes.clone();
                            // Set SPENT flag (flags byte is at index 10 in StoredEntry layout)
                            spent_prev_entry[10] |= AddrHistRecord::FLAG_SPENT;
                            // Recompute checksum over bytes 1..19 as StoredEntryFixed expects.
                            let checksum = StoredEntryFixed::<AddrEventBytes>::blake2b256(
                                &[&prev_addr_bytes, &spent_prev_entry[1..19]].concat(),
                            );
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

    // *** Internal DB methods ***
}
