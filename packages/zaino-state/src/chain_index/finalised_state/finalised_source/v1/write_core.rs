//! FinalisedState::V1 core write functionality.

use super::*;

/// Cheap heap-size estimate for a buffered [`IndexedBlock`], used only to bound the bulk-sync write
/// batch in [`DbV1::write_blocks_to_height`]. Exactness is not required — it just keeps the batch's
/// peak memory roughly within the configured budget.
#[cfg(not(feature = "transparent_address_history_experimental"))]
fn approx_indexed_block_bytes(block: &IndexedBlock) -> u64 {
    block
        .transactions()
        .iter()
        .map(|tx| {
            let transparent = tx.transparent();
            let items = transparent.inputs().len()
                + transparent.outputs().len()
                + tx.sapling().spends().len()
                + tx.sapling().outputs().len()
                + tx.orchard().actions().len();
            256 + items as u64 * 128
        })
        .sum()
}

/// Maximum number of blocks buffered in a single bulk-sync write batch, regardless of the byte
/// budget. Early-chain blocks are tiny, so the byte budget alone could buffer an enormous number of
/// blocks before the first commit — delaying durability/progress and inflating memory. This caps
/// the time-to-first-commit and the crash-loss window.
#[cfg(not(feature = "transparent_address_history_experimental"))]
const SYNC_WRITE_BATCH_MAX_BLOCKS: usize = 100_000;

/// Maximum wall-clock time spent buffering a single bulk-sync write batch before flushing, so
/// commits (and progress) happen regularly even when block fetches are slow.
#[cfg(not(feature = "transparent_address_history_experimental"))]
const SYNC_WRITE_BATCH_MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Interval between in-flight "syncing height" progress logs during bulk sync.
#[cfg(not(feature = "transparent_address_history_experimental"))]
const SYNC_PROGRESS_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
use crate::version;

/// [`DbWrite`] capability implementation for [`DbV1`].
///
/// This trait represents the mutating surface (append / delete tip / update metadata). Writes are
/// performed via LMDB write transactions and validated before becoming visible as “known-good”.
#[async_trait]
impl DbWrite for DbV1 {
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block(block).await
    }

    /// Bulk catch-up: ingests `tip+1..=height` from `source`, deferring txout-set accumulator
    /// maintenance across the run and rebuilding it once at the end. Each block is written with
    /// `update_tx_out_set = false` (deferred) and `validate = false` (the height is marked
    /// validated directly, since we built the block from the source this session).
    async fn write_blocks_to_height<S: crate::chain_index::source::BlockchainSource>(
        &self,
        height: Height,
        source: &S,
    ) -> Result<(), FinalisedStateError> {
        use crate::chain_index::finalised_state::build_indexed_block_from_source;
        use zebra_chain::parameters::NetworkUpgrade;

        let network = self.config.network;
        let zebra_network = network.to_zebra_network();
        let sapling_activation_height = NetworkUpgrade::Sapling
            .activation_height(&zebra_network)
            .expect("Sapling activation height must be set");
        let nu5_activation_height = NetworkUpgrade::Nu5.activation_height(&zebra_network);

        // Seed `parent_chainwork` from the current tip header (the block before the first one we
        // write). On an empty database this is genesis with zero chainwork. Read raw rather than via
        // `get_block_header`, which routes through `resolve_validated_hash_or_height` →
        // `validate_block_blocking` (a full re-validation for any height above `validated_tip`); the
        // tip is already on disk and trusted here, exactly as the v1.2 migration reads block data.
        let (start_height, mut parent_chainwork) = match self.tip_height().await? {
            None => (GENESIS_HEIGHT.0, crate::ChainWork::from_u256(0.into())),
            Some(tip) => {
                let tip_bytes = tip.to_bytes()?;
                let chainwork = tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    match ro.get(self.headers, &tip_bytes) {
                        Ok(raw) => {
                            let entry = StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                                .map_err(|e| {
                                    FinalisedStateError::Custom(format!(
                                        "tip header decode error: {e}"
                                    ))
                                })?;
                            Ok::<_, FinalisedStateError>(entry.inner().context.chainwork)
                        }
                        Err(lmdb::Error::NotFound) => Ok(crate::ChainWork::from_u256(0.into())),
                        Err(e) => Err(FinalisedStateError::LmdbError(e)),
                    }
                })?;
                (tip.0 + 1, chainwork)
            }
        };

        // Nothing to do when the tip already meets the target. Importantly, this means a steady-state
        // poll (the indexer calls `sync_to_height` repeatedly) does *not* trigger the bulk accumulator
        // rebuild below when no new blocks finalised.
        if start_height > height.0 {
            return Ok(());
        }

        info!(
            "write_blocks_to_height: syncing finalised blocks {start_height}..={} on {:?}",
            height.0, network
        );

        // Bulk path: buffer blocks up to a byte budget, then write the whole batch in one
        // transaction with the random-keyed `spent` / `txid_location` indexes inserted in sorted
        // order (sequential B-tree sweep instead of random faults once the DB exceeds RAM). The
        // address-history feature can't be batched (its prev-output resolution can't see
        // earlier-in-batch uncommitted blocks), so it keeps the per-block path.
        #[cfg(not(feature = "transparent_address_history_experimental"))]
        {
            let batch_budget = self.config.storage.database.sync_write_batch_bytes.max(1);
            let mut next = start_height;
            let mut last_progress_log = std::time::Instant::now();
            while next <= height.0 {
                // Fetch blocks (async; an LMDB write txn is `!Send` and cannot be held across the
                // await) into a buffer, flushing on the *first* of: byte budget, block-count cap, or
                // time cap. The count/time caps keep the first commit (and progress, and crash-loss
                // window) prompt even on the tiny early-chain blocks, where the byte budget alone
                // would buffer a huge number of blocks before committing.
                let mut batch: Vec<IndexedBlock> = Vec::new();
                let mut batch_bytes: u64 = 0;
                let batch_started = std::time::Instant::now();
                while next <= height.0
                    && batch_bytes < batch_budget
                    && batch.len() < SYNC_WRITE_BATCH_MAX_BLOCKS
                    && batch_started.elapsed() < SYNC_WRITE_BATCH_MAX_INTERVAL
                {
                    let block = build_indexed_block_from_source(
                        source,
                        network,
                        sapling_activation_height,
                        nu5_activation_height,
                        next,
                        parent_chainwork,
                    )
                    .await?;
                    parent_chainwork = block.context.chainwork;
                    batch_bytes = batch_bytes.saturating_add(approx_indexed_block_bytes(&block));
                    batch.push(block);
                    next += 1;

                    // In-flight progress: the block being fetched, throttled by time. (The committed
                    // tip is reported by the per-batch commit log below.)
                    if last_progress_log.elapsed() >= SYNC_PROGRESS_LOG_INTERVAL {
                        info!(
                            "write_blocks_to_height: syncing height {} / {} on {:?}",
                            next - 1,
                            height.0,
                            network
                        );
                        last_progress_log = std::time::Instant::now();
                    }
                }

                if batch.is_empty() {
                    break;
                }

                // Write + sort + commit the batch atomically, then force durability. The on-disk
                // `headers` tip never runs ahead of the indexes, so resume is gap-free.
                tokio::task::block_in_place(|| self.write_block_batch_blocking(&batch))?;
                tokio::task::block_in_place(|| self.env.sync(true)).map_err(|e| {
                    FinalisedStateError::Custom(format!("LMDB checkpoint sync failed: {e}"))
                })?;

                // Only after the batch is committed + synced do we advance the validated tip.
                for block in &batch {
                    self.mark_validated(block.context.index.height.0);
                }
                self.status.store(StatusType::Ready);
                info!(
                    "write_blocks_to_height: committed batch to height {} ({} blocks)",
                    next - 1,
                    batch.len()
                );
            }
        }
        #[cfg(feature = "transparent_address_history_experimental")]
        {
            for height_int in start_height..=height.0 {
                let block = build_indexed_block_from_source(
                    source,
                    network,
                    sapling_activation_height,
                    nu5_activation_height,
                    height_int,
                    parent_chainwork,
                )
                .await?;
                parent_chainwork = block.context.chainwork;

                self.write_block_with_options(block, false).await?;
            }
        }

        // Bring the deferred txout-set accumulator up to the new tip. The full from-genesis rebuild
        // is a fixed chain-length scan, so running it on every catch-up poll stalls the sync loop
        // once the chain is large. Use it only for the first build or an unusually large gap (e.g. a
        // sync interrupted far behind the on-disk tip); in steady state apply just the delta for the
        // blocks we wrote — O(range) work — which produces the identical accumulator at the tip.
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

    /// Writes a given (finalised) [`IndexedBlock`] to FinalisedState.
    ///
    /// Single-block append: the txout-set accumulator is maintained incrementally and the written
    /// block is validated before the height advances. Bulk catch-up uses
    /// [`DbV1::write_blocks_to_height`], which defers the accumulator (see
    /// [`DbV1::write_block_with_options`]) and rebuilds it once at the tip.
    ///
    /// NOTE: This method should never leave a block partially written to the database.
    pub(crate) async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block_with_options(block, true).await
    }

    /// Writes a single finalised [`IndexedBlock`], optionally maintaining the txout-set accumulator.
    ///
    /// - `update_tx_out_set`: when `true`, the accumulator is recomputed incrementally for this
    ///   block and persisted (with its freshness watermark) inside the block's write transaction.
    ///   When `false`, accumulator maintenance is deferred — the caller is responsible for a bulk
    ///   rebuild (see [`DbV1::rebuild_tx_out_set_accumulator`]).
    ///
    /// Validation is *not* a read-back pass on this path. Two cheap in-memory correctness checks run
    /// before commit — parent-hash continuity (tip-check) and the header merkle root (vs. the
    /// block's txids) — after which the height is marked validated directly. The expensive
    /// [`DbV1::validate_block_blocking`] re-read is reserved for startup, where on-disk data is
    /// untrusted.
    ///
    /// NOTE: This method should never leave a block partially written to the database.
    // `u32::is_multiple_of` is only stable from Rust 1.87; the `% 100 == 0` form below keeps the
    // crate buildable on our older minimum supported Rust version.
    #[allow(clippy::manual_is_multiple_of)]
    async fn write_block_with_options(
        &self,
        block: IndexedBlock,
        update_tx_out_set: bool,
    ) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);
        let block_hash = block.context.index.hash;
        let block_hash_bytes = block_hash.to_bytes()?;
        let block_height = block.context.index.height;
        let block_height_bytes = block_height.to_bytes()?;

        // Check if this specific block already exists (idempotent write support for shared DB).
        // This handles the case where multiple processes share the same FinalisedState.
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
                Ok((last_height_bytes, last_header_bytes)) => {
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

                    // Parent-hash continuity: the new block must extend the current tip. This is
                    // one of the two cheap, in-memory correctness checks (the other is the merkle
                    // root below) that justify marking the block validated after a successful write
                    // without the expensive post-commit re-read.
                    let last_entry = StoredEntryVar::<BlockHeaderData>::from_bytes(
                        last_header_bytes,
                    )
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!(
                            "tip header decode error during continuity check: {e}"
                        ))
                    })?;
                    if last_entry.inner().context.hash() != block.context.parent_hash() {
                        return Err(FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: format!(
                                "parent hash does not extend current tip (tip: {:?}, parent: {:?})",
                                last_entry.inner().context.hash(),
                                block.context.parent_hash()
                            ),
                        });
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
                "Block {} at height {} already exists in FinalisedState, skipping write.",
                &block_hash, &block_height.0
            );
            return Ok(());
        }

        // Build DBHeight
        let height_entry = StoredEntryFixed::new(&block_hash_bytes, block.context.index.height);

        // Build header
        let header_entry = StoredEntryVar::new(
            &block_height_bytes,
            BlockHeaderData::new(block.context, *block.data()),
        );

        // Build commitment tree data
        let commitment_tree_entry =
            StoredEntryFixed::new(&block_height_bytes, *block.commitment_tree_data());

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
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut sapling = Vec::with_capacity(tx_len);
        let mut orchard = Vec::with_capacity(tx_len);

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

            if !txid_set.insert(*hash) {
                return Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("duplicate transaction hash in block: {hash:?}"),
                });
            }

            // Transparent transactions — paired with the txid at the source binding.
            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transactions.push((*hash, transparent_data));

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

            // Transaction location
            let tx_index =
                u16::try_from(_tx_index).map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: format!("transaction index {_tx_index} does not fit into u16"),
                })?;

            let tx_location = TxLocation::new(block_height.into(), tx_index);

            // Transparent Inputs: Build Spent Outpoints Index
            for input in tx.transparent().inputs().iter() {
                if input.is_null_prevout() {
                    continue;
                }

                let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                if spent_map.insert(prev_outpoint, tx_location).is_some() {
                    return Err(FinalisedStateError::InvalidBlock {
                        height: block_height.0,
                        hash: block_hash,
                        reason: format!(
                            "duplicate transparent spend for outpoint {prev_outpoint:?}"
                        ),
                    });
                }
            }

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
                    if txid_set.contains(&prev_tx_hash) {
                        // Locate the paired (txid, transparent_data) within this block.
                        if let Some((tx_index, (_, Some(prev_transparent)))) = transactions
                            .iter()
                            .enumerate()
                            .find(|(_, (h, _))| h == &prev_tx_hash)
                        {
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

        // Accumulator maintenance is deferred on the bulk-sync path (`update_tx_out_set == false`)
        // and rebuilt once at the tip; only the single-block append path maintains it incrementally.
        let tx_out_set_info_accumulator = if update_tx_out_set {
            Some(
                self.calculate_tx_out_set_info_accumulator_after_block(
                    block_height,
                    &transactions,
                    &spent_map,
                )
                .await?,
            )
        } else {
            None
        };

        // Split the paired vector into the per-table shapes used for storage.
        let (txids, transparent): (Vec<TransactionHash>, Vec<Option<TransparentCompactTx>>) =
            transactions.into_iter().unzip();

        // Cheap, in-memory correctness check: the block's txids must reproduce the header's merkle
        // root. Together with the parent-hash continuity check in the tip-check above, this is what
        // lets us mark the block validated after a successful write without the expensive
        // post-commit re-read + spent-index cross-check (which only re-verifies on-disk integrity of
        // bytes we just wrote from memory, and is redundant for our own writes).
        {
            let txid_bytes: Vec<[u8; 32]> = txids.iter().map(|txid| txid.0).collect();
            let computed_merkle_root = Self::calculate_block_merkle_root(&txid_bytes);
            if &computed_merkle_root != block.data().merkle_root() {
                return Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: "header merkle root does not match block txids".to_string(),
                });
            }
        }

        // Reverse txid index entries (`txid -> TxLocation`). Built before `txids` is moved into
        // the `TxidList` below, and sorted by txid so the random-keyed `txid_location` B-tree
        // sees locally-ordered inserts.
        let mut txid_location_entries: Vec<([u8; 32], TxLocation)> =
            Vec::with_capacity(txids.len());
        for (tx_index, txid) in txids.iter().enumerate() {
            let tx_index = u16::try_from(tx_index).map_err(|_| {
                FinalisedStateError::Custom(format!(
                    "transaction index out of range at height {}",
                    block_height.0
                ))
            })?;
            txid_location_entries.push(((*txid).into(), TxLocation::new(block_height.0, tx_index)));
        }
        txid_location_entries.sort_by_key(|entry| entry.0);

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
            spent: self.spent,
            txid_location: self.txid_location,
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        let join_handle = tokio::task::spawn_blocking(move || {
            // Write block to FinalisedState
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

            // Reverse txid index: `txid -> TxLocation`.
            for (txid_bytes, tx_location) in &txid_location_entries {
                let entry_bytes = StoredEntryFixed::new(txid_bytes, *tx_location).to_bytes()?;
                txn.put(
                    zaino_db.txid_location,
                    txid_bytes,
                    &entry_bytes,
                    WriteFlags::NO_OVERWRITE,
                )?;
            }

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

            // Write spent to FinalisedState
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

            // Persist the incrementally-maintained accumulator and advance its freshness watermark
            // in the same transaction as the block, so the watermark always tracks the height the
            // accumulator reflects. Skipped on the deferred (bulk-sync) path.
            if let Some(tx_out_set_info_accumulator) = tx_out_set_info_accumulator {
                let tx_out_set_info_accumulator_entry = StoredEntryFixed::new(
                    TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                    tx_out_set_info_accumulator,
                );

                txn.put(
                    zaino_db.tx_out_set_info_accumulator,
                    &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                    &tx_out_set_info_accumulator_entry.to_bytes()?,
                    WriteFlags::empty(),
                )?;

                let watermark =
                    StoredEntryFixed::new(TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY, block_height);
                txn.put(
                    zaino_db.metadata,
                    &TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
                    &watermark.to_bytes()?,
                    WriteFlags::empty(),
                )?;
            }

            #[cfg(feature = "transparent_address_history_experimental")]
            {
                // Write outputs to FinalisedState addrhist
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

                // Write inputs to FinalisedState addrhist
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

            // `txn.commit()` makes the block visible to subsequent readers (LMDB MVCC), but the
            // env is opened with `NO_SYNC`, so it is *not* fsynced here. Durability is forced at
            // `SYNC_CHECKPOINT_INTERVAL` boundaries below and on graceful shutdown.
            txn.commit()?;

            // Advance the validated tip directly. The block was built from a trusted source this
            // session and passed the cheap in-memory correctness checks (parent-hash continuity and
            // merkle root) before commit, so the expensive read-back validation pass is redundant
            // here — it only re-verifies on-disk integrity of bytes we just wrote. Startup remains
            // the integrity gate for untrusted on-disk data (`validate_block_blocking` via
            // `initial_block_scan`). Marking validated keeps reads on the `is_validated` fast path.
            zaino_db.mark_validated(block_height.0);

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
                // The block was committed inside the blocking task above. Under `NO_SYNC` that
                // commit is not yet on disk; force a durability checkpoint every
                // `SYNC_CHECKPOINT_INTERVAL` blocks so a crash can only lose a bounded tail (which
                // the syncer re-fetches from the on-disk tip). Genesis is checkpointed too so a
                // brand-new cache has a durable root.
                self.status.store(StatusType::Ready);
                if block_height.0 % SYNC_CHECKPOINT_INTERVAL == 0 {
                    tokio::task::block_in_place(|| self.env.sync(true)).map_err(|e| {
                        FinalisedStateError::Custom(format!("LMDB checkpoint sync failed: {e}"))
                    })?;
                }
                if block.context.index.height.0 % 100 == 0 {
                    info!(
                        "Successfully committed block {} at height {} to FinalisedState.",
                        &block.context.index.hash, &block.context.index.height
                    );
                } else {
                    tracing::debug!(
                        "Successfully committed block {} at height {} to FinalisedState.",
                        &block.context.index.hash,
                        &block.context.index.height
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
                            if stored_header.context.index.hash == block_hash {
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

    /// Writes a batch of strictly-consecutive blocks in a single LMDB transaction, inserting the
    /// random-keyed `spent` and `txid_location` entries in **sorted key order**.
    ///
    /// Once the DB exceeds RAM, the per-block path's random-key inserts fault a different B-tree
    /// leaf almost every time. Buffering a batch and inserting its index entries in ascending key
    /// order turns that into a sequential sweep — each leaf is faulted in once, filled, and written
    /// once. The height-keyed tables (`headers`/`txids`/`transparent`/`sapling`/`orchard`/
    /// `commitment_tree_data`) are written per block in ascending height (already sequential).
    ///
    /// Crash-safety: the whole batch — every block's height-keyed tables **and** the batch's sorted
    /// index entries — commits in one transaction, so the `headers` tip never runs ahead of the
    /// indexes. A crash discards the uncommitted batch (and, under `NO_SYNC`, rolls back to the last
    /// `env.sync`), leaving a consistent prefix the syncer resumes from. The txout-set accumulator
    /// is **not** maintained here (deferred; the caller rebuilds it at the tip).
    ///
    /// Heights must be strictly consecutive and extend the current tip; this is the bulk-sync path,
    /// which assumes a single writer. Re-writing identical data on resume is a no-op (idempotent
    /// puts), so it is safe to re-run after an interrupted sync.
    ///
    /// WARNING: blocking — call from a blocking context. Only built when the experimental
    /// address-history feature is **off**; that feature uses the per-block path (its prev-output
    /// resolution cannot see earlier-in-batch uncommitted blocks).
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    pub(crate) fn write_block_batch_blocking(
        &self,
        blocks: &[IndexedBlock],
    ) -> Result<(), FinalisedStateError> {
        use lmdb::Transaction as _;

        if blocks.is_empty() {
            return Ok(());
        }

        // Idempotent `NO_OVERWRITE` put: a re-seen key whose stored bytes are identical is a no-op
        // (safe resume); a key with conflicting bytes is a genuine error.
        fn put_idempotent(
            txn: &mut lmdb::RwTransaction<'_>,
            db: Database,
            key: &[u8],
            value: &[u8],
        ) -> Result<(), FinalisedStateError> {
            match txn.put(db, &key, &value, WriteFlags::NO_OVERWRITE) {
                Ok(()) => Ok(()),
                Err(lmdb::Error::KeyExist) => {
                    let existing = txn.get(db, &key).map_err(FinalisedStateError::LmdbError)?;
                    if existing == value {
                        Ok(())
                    } else {
                        Err(FinalisedStateError::Custom(
                            "conflicting existing entry during batched block write".to_string(),
                        ))
                    }
                }
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        }

        self.status.store(StatusType::Syncing);
        let mut txn = self.env.begin_rw_txn()?;

        // Seed the continuity chain from the current on-disk tip (genesis if empty).
        let (mut prev_height, mut prev_hash): (Option<u32>, Option<BlockHash>) = {
            let cursor = txn.open_ro_cursor(self.headers)?;
            match cursor.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((last_height_bytes, last_header_bytes)) => {
                    let last_height = Height::from_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?;
                    let last_entry = StoredEntryVar::<BlockHeaderData>::from_bytes(
                        last_header_bytes,
                    )
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!("tip header decode error: {e}"))
                    })?;
                    (
                        Some(last_height.0),
                        Some(*last_entry.inner().context.hash()),
                    )
                }
                Err(lmdb::Error::NotFound) => (None, None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }
        };

        // Batch-level collectors for the random-keyed indexes; sorted before insertion below.
        let mut spent_batch: Vec<(Vec<u8>, TxLocation)> = Vec::new();
        let mut txid_location_batch: Vec<([u8; 32], TxLocation)> = Vec::new();

        for block in blocks {
            let block_hash = block.context.index.hash;
            let block_hash_bytes = block_hash.to_bytes()?;
            let block_height = block.context.index.height;
            let block_height_bytes = block_height.to_bytes()?;

            // Continuity: height = prev + 1 and parent extends the current tip (genesis if empty).
            match prev_height {
                Some(tip) => {
                    if block_height.0 != tip + 1 {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot write block at height {block_height:?}; current tip is {tip}"
                        )));
                    }
                    if Some(*block.context.parent_hash()) != prev_hash {
                        return Err(FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: "parent hash does not extend current tip".to_string(),
                        });
                    }
                }
                None => {
                    if block_height.0 != GENESIS_HEIGHT.0 {
                        return Err(FinalisedStateError::Custom(format!(
                            "first block must be height 0, got {block_height:?}"
                        )));
                    }
                }
            }

            let height_entry = StoredEntryFixed::new(&block_hash_bytes, block_height);
            let header_entry = StoredEntryVar::new(
                &block_height_bytes,
                BlockHeaderData::new(block.context, *block.data()),
            );
            let commitment_tree_entry =
                StoredEntryFixed::new(&block_height_bytes, *block.commitment_tree_data());

            let tx_len = block.transactions().len();
            let mut txids: Vec<TransactionHash> = Vec::with_capacity(tx_len);
            let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
            let mut transparent: Vec<Option<TransparentCompactTx>> = Vec::with_capacity(tx_len);
            let mut sapling = Vec::with_capacity(tx_len);
            let mut orchard = Vec::with_capacity(tx_len);

            for (tx_index, tx) in block.transactions().iter().enumerate() {
                let hash = tx.txid();
                if !txid_set.insert(*hash) {
                    return Err(FinalisedStateError::InvalidBlock {
                        height: block_height.0,
                        hash: block_hash,
                        reason: format!("duplicate transaction hash in block: {hash:?}"),
                    });
                }
                txids.push(*hash);

                let transparent_data = if tx.transparent().inputs().is_empty()
                    && tx.transparent().outputs().is_empty()
                {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
                transparent.push(transparent_data);

                let sapling_data =
                    if tx.sapling().spends().is_empty() && tx.sapling().outputs().is_empty() {
                        None
                    } else {
                        Some(tx.sapling().clone())
                    };
                sapling.push(sapling_data);

                let orchard_data = if tx.orchard().actions().is_empty() {
                    None
                } else {
                    Some(tx.orchard().clone())
                };
                orchard.push(orchard_data);

                let tx_index =
                    u16::try_from(tx_index).map_err(|_| FinalisedStateError::InvalidBlock {
                        height: block_height.0,
                        hash: block_hash,
                        reason: format!("transaction index {tx_index} does not fit into u16"),
                    })?;
                let tx_location = TxLocation::new(block_height.0, tx_index);

                for input in tx.transparent().inputs().iter() {
                    if input.is_null_prevout() {
                        continue;
                    }
                    let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                    spent_batch.push((prev_outpoint.to_bytes()?, tx_location));
                }

                txid_location_batch.push(((*hash).into(), tx_location));
            }

            // Cheap in-memory correctness check: txids must reproduce the header merkle root.
            let txid_bytes: Vec<[u8; 32]> = txids.iter().map(|t| t.0).collect();
            let computed_merkle_root = Self::calculate_block_merkle_root(&txid_bytes);
            if &computed_merkle_root != block.data().merkle_root() {
                return Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: "header merkle root does not match block txids".to_string(),
                });
            }

            let txid_entry = StoredEntryVar::new(&block_height_bytes, TxidList::new(txids));
            let transparent_entry =
                StoredEntryVar::new(&block_height_bytes, TransparentTxList::new(transparent));
            let sapling_entry =
                StoredEntryVar::new(&block_height_bytes, SaplingTxList::new(sapling));
            let orchard_entry =
                StoredEntryVar::new(&block_height_bytes, OrchardTxList::new(orchard));

            // Height-keyed tables (+ the hash-keyed `heights`, one entry/block) written per block.
            put_idempotent(
                &mut txn,
                self.headers,
                &block_height_bytes,
                &header_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.heights,
                &block_hash_bytes,
                &height_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.txids,
                &block_height_bytes,
                &txid_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.transparent,
                &block_height_bytes,
                &transparent_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.sapling,
                &block_height_bytes,
                &sapling_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.orchard,
                &block_height_bytes,
                &orchard_entry.to_bytes()?,
            )?;
            put_idempotent(
                &mut txn,
                self.commitment_tree_data,
                &block_height_bytes,
                &commitment_tree_entry.to_bytes()?,
            )?;

            prev_height = Some(block_height.0);
            prev_hash = Some(block_hash);
        }

        // Insert the random-keyed indexes in ascending key order so the B-tree is swept
        // sequentially rather than faulting on scattered leaves. A duplicate key with a
        // conflicting value (a double-spend / inconsistency) is rejected by `put_idempotent`.
        spent_batch.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, tx_location) in &spent_batch {
            let entry_bytes = StoredEntryFixed::new(key, *tx_location).to_bytes()?;
            put_idempotent(&mut txn, self.spent, key, &entry_bytes)?;
        }

        txid_location_batch.sort_by_key(|entry| entry.0);
        for (key, tx_location) in &txid_location_batch {
            let entry_bytes = StoredEntryFixed::new(key, *tx_location).to_bytes()?;
            put_idempotent(&mut txn, self.txid_location, key, &entry_bytes)?;
        }

        txn.commit()?;
        Ok(())
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
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);

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
                // Transparent transactions — paired with the txid at the source binding.
                let transparent_data = if tx.transparent().inputs().is_empty()
                    && tx.transparent().outputs().is_empty()
                {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
                transactions.push((*hash, transparent_data));
            }

            // Transaction location
            let tx_location = TxLocation::new(block_height.into(), _tx_index as u16);

            // Build Spent Outpoints Index
            for input in tx.transparent().inputs().iter() {
                if input.is_null_prevout() {
                    continue;
                }

                let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                if spent_map.insert(prev_outpoint, tx_location).is_some() {
                    return Err(FinalisedStateError::InvalidBlock {
                        height: block_height.0,
                        hash: block_hash,
                        reason: format!(
                            "duplicate transparent spend for outpoint {prev_outpoint:?}"
                        ),
                    });
                }
            }

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

                    //Check if output is in *this* block, else fetch from DB.
                    let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                    if txid_set.contains(&prev_tx_hash) {
                        // Locate the paired (txid, transparent_data) within this block.
                        if let Some((tx_index, (_, Some(prev_transparent)))) = transactions
                            .iter()
                            .enumerate()
                            .find(|(_, (h, _))| h == &prev_tx_hash)
                        {
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

        let tx_out_set_info_accumulator = self
            .calculate_tx_out_set_info_accumulator_after_delete_block(&transactions, &spent_map)
            .await?;

        // Reverse txid index keys written for this block by `write_block`.
        let txid_location_keys: Vec<[u8; 32]> = transactions
            .iter()
            .map(|(txid, _)| (*txid).into())
            .collect();

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
            spent: self.spent,
            txid_location: self.txid_location,
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        };
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
}

#[cfg(test)]
impl DbV1 {
    /// Returns the current contiguous validated-tip height. Test hook for asserting that the write
    /// path advances the validated tip without relying on the background validator.
    pub(crate) fn validated_tip_height(&self) -> u32 {
        self.validated_tip
            .load(std::sync::atomic::Ordering::Acquire)
    }

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

            let sapling_data =
                if tx.sapling().spends().is_empty() && tx.sapling().outputs().is_empty() {
                    None
                } else {
                    Some(tx.sapling().clone())
                };
            sapling.push(sapling_data);

            let orchard_data = if tx.orchard().actions().is_empty() {
                None
            } else {
                Some(tx.orchard().clone())
            };
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
