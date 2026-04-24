//! ZainoDB::V1 compact block indexing functionality.

use super::*;

/// [`CompactBlockExt`] capability implementation for [`DbV1`].
///
/// Exposes `zcash_client_backend`-compatible compact blocks derived from stored header + shielded
/// transaction data.
#[async_trait]
impl CompactBlockExt for DbV1 {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.get_compact_block(height, pool_types).await
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        self.get_compact_block_stream(start_height, end_height, pool_types)
            .await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the CompactBlock for the given Height.
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            // ----- Fetch Header -----
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let header: BlockHeaderData = *StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?
                .inner();

            // ----- Fetch Txids -----
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let txids_stored_entry_var = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?;
            let txids = txids_stored_entry_var.inner().txids();

            // ----- Fetch Transparent Tx Data -----
            let transparent_stored_entry_var = if pool_types.includes_transparent() {
                let raw = match txn.get(self.transparent, &height_bytes) {
                    Ok(val) => val,
                    Err(lmdb::Error::NotFound) => {
                        return Err(FinalisedStateError::DataUnavailable(
                            "block data missing from db".into(),
                        ));
                    }
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                };

                Some(
                    StoredEntryVar::<TransparentTxList>::from_bytes(raw).map_err(|e| {
                        FinalisedStateError::Custom(format!("transparent decode error: {e}"))
                    })?,
                )
            } else {
                None
            };
            let transparent = match transparent_stored_entry_var.as_ref() {
                Some(stored_entry_var) => stored_entry_var.inner().tx(),
                None => &[],
            };

            // ----- Fetch Sapling Tx Data -----
            let sapling_stored_entry_var = if pool_types.includes_sapling() {
                let raw = match txn.get(self.sapling, &height_bytes) {
                    Ok(val) => val,
                    Err(lmdb::Error::NotFound) => {
                        return Err(FinalisedStateError::DataUnavailable(
                            "block data missing from db".into(),
                        ));
                    }
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                };

                Some(
                    StoredEntryVar::<SaplingTxList>::from_bytes(raw).map_err(|e| {
                        FinalisedStateError::Custom(format!("sapling decode error: {e}"))
                    })?,
                )
            } else {
                None
            };
            let sapling = match sapling_stored_entry_var.as_ref() {
                Some(stored_entry_var) => stored_entry_var.inner().tx(),
                None => &[],
            };

            // ----- Fetch Orchard Tx Data -----
            let orchard_stored_entry_var = if pool_types.includes_orchard() {
                let raw = match txn.get(self.orchard, &height_bytes) {
                    Ok(val) => val,
                    Err(lmdb::Error::NotFound) => {
                        return Err(FinalisedStateError::DataUnavailable(
                            "block data missing from db".into(),
                        ));
                    }
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                };

                Some(
                    StoredEntryVar::<OrchardTxList>::from_bytes(raw).map_err(|e| {
                        FinalisedStateError::Custom(format!("orchard decode error: {e}"))
                    })?,
                )
            } else {
                None
            };
            let orchard = match orchard_stored_entry_var.as_ref() {
                Some(stored_entry_var) => stored_entry_var.inner().tx(),
                None => &[],
            };

            // ----- Construct CompactTx -----
            let vtx: Vec<zaino_proto::proto::compact_formats::CompactTx> = txids
                .iter()
                .enumerate()
                .filter_map(|(i, txid)| {
                    let spends = sapling
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|s| {
                            s.spends()
                                .iter()
                                .map(|sp| sp.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let outputs = sapling
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|s| {
                            s.outputs()
                                .iter()
                                .map(|o| o.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let actions = orchard
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|o| {
                            o.actions()
                                .iter()
                                .map(|a| a.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let (vin, vout) = transparent
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|t| (t.compact_vin(), t.compact_vout()))
                        .unwrap_or_default();

                    // Omit transactions that have no elements in any requested pool type.
                    //
                    // This keeps `vtx` compact (it only contains transactions relevant to the caller’s pool filter),
                    // but it also means:
                    // - `vtx.len()` may be smaller than the block transaction count, and
                    // - transaction indices in `vtx` may be non-contiguous.
                    // Consumers must use `CompactTx.index` (the original transaction position in the block) rather
                    // than assuming `vtx` preserves block order densely.
                    //
                    // TODO: Re-evaluate whether omitting "empty-for-filter" transactions is the desired API behaviour.
                    //       Some clients may expect a position-preserving representation (one entry per txid), even if
                    //       the per-pool fields are empty for a given filter.
                    if spends.is_empty()
                        && outputs.is_empty()
                        && actions.is_empty()
                        && vin.is_empty()
                        && vout.is_empty()
                    {
                        return None;
                    }

                    Some(zaino_proto::proto::compact_formats::CompactTx {
                        index: i as u64,
                        txid: txid.0.to_vec(),
                        fee: 0,
                        spends,
                        outputs,
                        actions,
                        vin,
                        vout,
                    })
                })
                .collect();

            // ----- Fetch Commitment Tree Data -----
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let commitment_tree_data: CommitmentTreeData = *StoredEntryFixed::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                })?
                .inner();

            let chain_metadata = zaino_proto::proto::compact_formats::ChainMetadata {
                sapling_commitment_tree_size: commitment_tree_data.sizes().sapling(),
                orchard_commitment_tree_size: commitment_tree_data.sizes().orchard(),
            };

            // ----- Construct CompactBlock -----
            Ok(zaino_proto::proto::compact_formats::CompactBlock {
                proto_version: 4,
                height: header.context.height().0 as u64,
                hash: header.context.hash().0.to_vec(),
                prev_hash: header.context.parent_hash().0.to_vec(),
                // Is this safe?
                time: header.data().time() as u32,
                header: Vec::new(),
                vtx,
                chain_metadata: Some(chain_metadata),
            })
        })
    }

    /// Streams `CompactBlock` messages for an inclusive height range.
    ///
    /// This implementation is designed for high-throughput lightclient serving:
    /// - It performs a single cursor-walk over the headers database and keeps all other databases
    ///   (txids + optional pool-specific tx data + commitment tree data) strictly aligned to the
    ///   same LMDB key.
    /// - It uses *short-lived* read transactions and periodically re-seeks by key, which:
    ///   - reduces the lifetime of LMDB reader slots,
    ///   - bounds the amount of data held in the same read snapshot,
    ///   - and prevents a single long stream from monopolising the environment’s read resources.
    ///
    /// Ordering / range semantics:
    /// - The stream covers the inclusive range `[start_height, end_height]`.
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    /// - This function enforces *contiguous heights* in the headers database. Missing heights, key
    ///   ordering problems, or cursor desynchronisation are treated as internal errors because they
    ///   indicate database corruption or a violated storage invariant.
    ///
    /// Pool filtering:
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    ///
    /// Concurrency model:
    /// - Spawns a dedicated blocking task (`spawn_blocking`) which performs LMDB reads and decoding.
    /// - Results are pushed into a bounded `mpsc` channel; backpressure is applied if the consumer
    ///   is slow.
    ///
    /// Errors:
    /// - Database-missing conditions are sent downstream as `tonic::Status::not_found`.
    /// - Decode failures, cursor desynchronisation, and invariant violations are sent as
    ///   `tonic::Status::internal`.
    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        // Do NOT validate the whole requested range up-front here.
        // Validate heights on-demand inside the blocking task so we can return
        // the stream handle immediately and start sending blocks as they become ready.
        //
        // Preserve caller ordering: direction is derived from the caller-supplied heights.
        let validated_start_height = start_height;
        let validated_end_height = end_height;

        let start_key_bytes = validated_start_height.to_bytes()?;

        // Direction is derived from the validated heights. This relies on `validate_block_range`
        // preserving input ordering (i.e. not normalising to (min, max)).
        let is_ascending = validated_start_height <= validated_end_height;

        // Bounded channel provides backpressure so the blocking task cannot run unbounded ahead of
        // the gRPC consumer.
        //
        // TODO: Investigate whether channel size should be changed, added to config, or set dynamically base on resources.
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<Result<CompactBlock, tonic::Status>>(128);

        // Clone everything the blocking task needs so we can move it into the blocking closure.
        // This mirrors patterns already used elsewhere in this module.
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
            /// Maximum number of blocks to stream per LMDB read transaction.
            ///
            /// The cursor-walk is resumed by re-seeking to the next expected height key. This keeps
            /// read transactions short-lived and reduces pressure on LMDB reader slots.
            const BLOCKS_PER_READ_TRANSACTION: usize = 1024;

            // =====================================================================================
            // Helper functions
            // =====================================================================================
            //
            // These helpers keep the main streaming loop readable and ensure that any failure:
            // - emits exactly one `tonic::Status` into the stream (best-effort), and then
            // - terminates the blocking task.
            //
            // They intentionally return `Option`/`Result` to allow early-exit with minimal boilerplate.

            /// Send a `tonic::Status` downstream and ignore send errors.
            ///
            /// A send error means the receiver side has been dropped (e.g. client cancelled the RPC),
            /// so the producer should terminate promptly.
            fn send_status(
                sender: &tokio::sync::mpsc::Sender<Result<CompactBlock, tonic::Status>>,
                status: tonic::Status,
            ) {
                let _ = sender.blocking_send(Err(status));
            }

            /// Open a read-only cursor for `database` inside `txn`.
            ///
            /// On failure, emits an internal status and returns `None`.
            fn open_ro_cursor_or_send<'txn>(
                sender: &tokio::sync::mpsc::Sender<Result<CompactBlock, tonic::Status>>,
                txn: &'txn lmdb::RoTransaction<'txn>,
                database: lmdb::Database,
                database_name: &'static str,
            ) -> Option<lmdb::RoCursor<'txn>> {
                match txn.open_ro_cursor(database) {
                    Ok(cursor) => Some(cursor),
                    Err(error) => {
                        send_status(
                            sender,
                            tonic::Status::internal(format!(
                                "lmdb open_ro_cursor({database_name}) failed: {error}"
                            )),
                        );
                        None
                    }
                }
            }

            /// Position `cursor` exactly at `requested_key` using `MDB_SET_KEY`.
            ///
            /// Returns the `(key, value)` pair at that key. The returned `key` is expected to equal
            /// `requested_key` (the function enforces this).
            ///
            /// Some LMDB bindings occasionally return `Ok((None, value))` for cursor operations. When
            /// that happens:
            /// - If `verify_on_none_key` is true, we call `MDB_GET_CURRENT` once to recover and verify
            ///   the current key.
            /// - Otherwise we assume the cursor is correctly positioned and return `(requested_key, value)`.
            ///
            /// On `NotFound`, emits `not_found_status`. On other failures or verification failure, emits
            /// `internal(...)`. In all error cases it returns `None`.
            fn cursor_set_key_or_send<'txn>(
                sender: &tokio::sync::mpsc::Sender<Result<CompactBlock, tonic::Status>>,
                cursor: &lmdb::RoCursor<'txn>,
                requested_key: &'txn [u8],
                cursor_name: &'static str,
                not_found_status: tonic::Status,
                verify_on_none_key: bool,
            ) -> Option<(&'txn [u8], &'txn [u8])> {
                match cursor.get(Some(requested_key), None, lmdb_sys::MDB_SET_KEY) {
                    Ok((Some(found_key), found_val)) => {
                        if found_key != requested_key {
                            send_status(
                                sender,
                                tonic::Status::internal(format!(
                                    "lmdb SET_KEY({cursor_name}) returned non-matching key"
                                )),
                            );
                            None
                        } else {
                            Some((found_key, found_val))
                        }
                    }
                    Ok((None, found_val)) => {
                        // Some builds / bindings can return None for the key for certain ops. If requested,
                        // verify the cursor actually landed on the requested key via GET_CURRENT.
                        if verify_on_none_key {
                            let (recovered_key_opt, recovered_val) =
                                match cursor.get(None, None, lmdb_sys::MDB_GET_CURRENT) {
                                    Ok(pair) => pair,
                                    Err(error) => {
                                        send_status(
                                            sender,
                                            tonic::Status::internal(format!(
                                            "lmdb cursor GET_CURRENT({cursor_name}) failed: {error}"
                                        )),
                                        );
                                        return None;
                                    }
                                };

                            let recovered_key = match recovered_key_opt {
                                Some(key) => key,
                                None => {
                                    send_status(
                                        sender,
                                        tonic::Status::internal(format!(
                                            "lmdb GET_CURRENT({cursor_name}) returned no key"
                                        )),
                                    );
                                    return None;
                                }
                            };

                            if recovered_key != requested_key {
                                send_status(
                                sender,
                                tonic::Status::internal(format!(
                                    "lmdb SET_KEY({cursor_name}) landed on unexpected key: expected {:?}, got {:?}",
                                    requested_key,
                                    recovered_key,
                                )),
                            );
                                return None;
                            }

                            Some((recovered_key, recovered_val))
                        } else {
                            // Assume SET_KEY success implies match; return the requested key + value.
                            Some((requested_key, found_val))
                        }
                    }
                    Err(lmdb::Error::NotFound) => {
                        send_status(sender, not_found_status);
                        None
                    }
                    Err(error) => {
                        send_status(
                            sender,
                            tonic::Status::internal(format!(
                                "lmdb cursor SET_KEY({cursor_name}) failed: {error}"
                            )),
                        );
                        None
                    }
                }
            }

            /// Step the headers cursor using `step_op` and return the next `(key, value)` pair.
            ///
            /// This is special-cased because the headers cursor is the *driving cursor*; all other
            /// cursors must remain aligned to whatever key the headers cursor moves to.
            ///
            /// Returns:
            /// - `Ok(Some((k, v)))` when the cursor moved successfully.
            /// - `Ok(None)` when the cursor reached the end (`NotFound`).
            /// - `Err(())` when an error status has been emitted and streaming must stop.
            #[allow(clippy::complexity)]
            fn headers_step_or_send<'txn>(
                sender: &tokio::sync::mpsc::Sender<Result<CompactBlock, tonic::Status>>,
                headers_cursor: &lmdb::RoCursor<'txn>,
                step_op: lmdb_sys::MDB_cursor_op,
            ) -> Result<Option<(&'txn [u8], &'txn [u8])>, ()> {
                match headers_cursor.get(None, None, step_op) {
                    Ok((Some(found_key), found_val)) => Ok(Some((found_key, found_val))),
                    Ok((None, _found_val)) => {
                        // Some bindings can return None for the key; recover via GET_CURRENT.
                        let (recovered_key_opt, recovered_val) =
                            match headers_cursor.get(None, None, lmdb_sys::MDB_GET_CURRENT) {
                                Ok(pair) => pair,
                                Err(error) => {
                                    send_status(
                                        sender,
                                        tonic::Status::internal(format!(
                                            "lmdb cursor GET_CURRENT(headers) failed: {error}"
                                        )),
                                    );
                                    return Err(());
                                }
                            };
                        let recovered_key = match recovered_key_opt {
                            Some(key) => key,
                            None => {
                                send_status(
                                    sender,
                                    tonic::Status::internal(
                                        "lmdb GET_CURRENT(headers) returned no key".to_string(),
                                    ),
                                );
                                return Err(());
                            }
                        };
                        Ok(Some((recovered_key, recovered_val)))
                    }
                    Err(lmdb::Error::NotFound) => Ok(None),
                    Err(error) => {
                        send_status(
                            sender,
                            tonic::Status::internal(format!(
                                "lmdb cursor step(headers) failed: {error}"
                            )),
                        );
                        Err(())
                    }
                }
            }

            /// Step a non-header cursor and enforce that it remains aligned to `expected_key`.
            ///
            /// The design invariant for this streamer is:
            /// - the headers cursor chooses the next key
            /// - every other cursor must produce a value at that *same* key (otherwise the per-height
            ///   databases are inconsistent or a cursor has desynchronised).
            ///
            /// Returns the value slice for `expected_key` on success.
            /// On `NotFound`, emits `not_found_status`.
            /// On key mismatch or other errors, emits an internal error.
            fn cursor_step_expect_key_or_send<'txn>(
                sender: &tokio::sync::mpsc::Sender<Result<CompactBlock, tonic::Status>>,
                cursor: &lmdb::RoCursor<'txn>,
                step_op: lmdb_sys::MDB_cursor_op,
                expected_key: &[u8],
                cursor_name: &'static str,
                not_found_status: tonic::Status,
            ) -> Option<&'txn [u8]> {
                match cursor.get(None, None, step_op) {
                    Ok((Some(found_key), found_val)) => {
                        if found_key != expected_key {
                            send_status(
                                sender,
                                tonic::Status::internal(format!(
                                "lmdb cursor desync({cursor_name}): expected key {:?}, got {:?}",
                                expected_key, found_key
                            )),
                            );
                            None
                        } else {
                            Some(found_val)
                        }
                    }
                    Ok((None, _found_val)) => {
                        // Some bindings can return None for the key; recover via GET_CURRENT.
                        let (recovered_key_opt, recovered_val) =
                            match cursor.get(None, None, lmdb_sys::MDB_GET_CURRENT) {
                                Ok(pair) => pair,
                                Err(error) => {
                                    send_status(
                                        sender,
                                        tonic::Status::internal(format!(
                                        "lmdb cursor GET_CURRENT({cursor_name}) failed: {error}"
                                    )),
                                    );
                                    return None;
                                }
                            };

                        let recovered_key = match recovered_key_opt {
                            Some(key) => key,
                            None => {
                                send_status(
                                    sender,
                                    tonic::Status::internal(format!(
                                        "lmdb GET_CURRENT({cursor_name}) returned no key"
                                    )),
                                );
                                return None;
                            }
                        };

                        if recovered_key != expected_key {
                            send_status(
                                sender,
                                tonic::Status::internal(format!(
                                "lmdb cursor desync({cursor_name}): expected key {:?}, got {:?}",
                                expected_key, recovered_key
                            )),
                            );
                            None
                        } else {
                            Some(recovered_val)
                        }
                    }
                    Err(lmdb::Error::NotFound) => {
                        send_status(sender, not_found_status);
                        None
                    }
                    Err(error) => {
                        send_status(
                            sender,
                            tonic::Status::internal(format!(
                                "lmdb cursor step({cursor_name}) failed: {error}"
                            )),
                        );
                        None
                    }
                }
            }

            // =====================================================================================
            // Blocking streaming loop
            // =====================================================================================

            let step_op = if is_ascending {
                lmdb_sys::MDB_NEXT
            } else {
                lmdb_sys::MDB_PREV
            };

            // Contiguous-height enforcement: we expect every emitted block to have exactly this height.
            // This catches missing heights and cursor ordering/key-encoding problems early.
            let mut expected_height = validated_start_height;

            // Key used to re-seek at the start of each transaction chunk.
            // This begins at the start height and advances by exactly one height per emitted block.
            let mut next_start_key_bytes: Vec<u8> = start_key_bytes;

            loop {
                // Stop once we have emitted the inclusive end height.
                if is_ascending {
                    if expected_height > validated_end_height {
                        return;
                    }
                } else if expected_height < validated_end_height {
                    return;
                }

                // Open a short-lived read transaction for this chunk.
                //
                // We intentionally drop the transaction regularly to keep reader slots available and
                // to avoid holding a single snapshot for very large streams.
                let txn = match zaino_db.env.begin_ro_txn() {
                    Ok(txn) => txn,
                    Err(error) => {
                        send_status(
                            &sender,
                            tonic::Status::internal(format!("lmdb begin_ro_txn failed: {error}")),
                        );
                        return;
                    }
                };

                // Open cursors. Headers is the driving cursor; all others must remain key-aligned.
                let headers_cursor =
                    match open_ro_cursor_or_send(&sender, &txn, zaino_db.headers, "headers") {
                        Some(cursor) => cursor,
                        None => return,
                    };

                let txids_cursor =
                    match open_ro_cursor_or_send(&sender, &txn, zaino_db.txids, "txids") {
                        Some(cursor) => cursor,
                        None => return,
                    };

                let transparent_cursor = if pool_types.includes_transparent() {
                    match open_ro_cursor_or_send(&sender, &txn, zaino_db.transparent, "transparent")
                    {
                        Some(cursor) => Some(cursor),
                        None => return,
                    }
                } else {
                    None
                };

                let sapling_cursor = if pool_types.includes_sapling() {
                    match open_ro_cursor_or_send(&sender, &txn, zaino_db.sapling, "sapling") {
                        Some(cursor) => Some(cursor),
                        None => return,
                    }
                } else {
                    None
                };

                let orchard_cursor = if pool_types.includes_orchard() {
                    match open_ro_cursor_or_send(&sender, &txn, zaino_db.orchard, "orchard") {
                        Some(cursor) => Some(cursor),
                        None => return,
                    }
                } else {
                    None
                };

                let commitment_tree_cursor = match open_ro_cursor_or_send(
                    &sender,
                    &txn,
                    zaino_db.commitment_tree_data,
                    "commitment_tree_data",
                ) {
                    Some(cursor) => cursor,
                    None => return,
                };

                // Position headers cursor at the start key for this chunk. This is the authoritative key
                // that all other cursors must align to.
                let (current_key, mut raw_header_bytes) = match cursor_set_key_or_send(
                    &sender,
                    &headers_cursor,
                    next_start_key_bytes.as_slice(),
                    "headers",
                    tonic::Status::not_found(format!(
                        "missing header at requested start height key {:?}",
                        next_start_key_bytes
                    )),
                    true, // verify-on-none-key
                ) {
                    Some(pair) => pair,
                    None => return,
                };

                // Align all other cursors to the exact same key.
                let (_txids_key, mut raw_txids_bytes) = match cursor_set_key_or_send(
                    &sender,
                    &txids_cursor,
                    current_key,
                    "txids",
                    tonic::Status::not_found("block data missing from db (txids)"),
                    true,
                ) {
                    Some(pair) => pair,
                    None => return,
                };

                let mut raw_transparent_bytes: Option<&[u8]> =
                    if let Some(cursor) = transparent_cursor.as_ref() {
                        let (_key, val) = match cursor_set_key_or_send(
                            &sender,
                            cursor,
                            current_key,
                            "transparent",
                            tonic::Status::not_found("block data missing from db (transparent)"),
                            true,
                        ) {
                            Some(pair) => pair,
                            None => return,
                        };
                        Some(val)
                    } else {
                        None
                    };

                let mut raw_sapling_bytes: Option<&[u8]> =
                    if let Some(cursor) = sapling_cursor.as_ref() {
                        let (_key, val) = match cursor_set_key_or_send(
                            &sender,
                            cursor,
                            current_key,
                            "sapling",
                            tonic::Status::not_found("block data missing from db (sapling)"),
                            true,
                        ) {
                            Some(pair) => pair,
                            None => return,
                        };
                        Some(val)
                    } else {
                        None
                    };

                let mut raw_orchard_bytes: Option<&[u8]> =
                    if let Some(cursor) = orchard_cursor.as_ref() {
                        let (_key, val) = match cursor_set_key_or_send(
                            &sender,
                            cursor,
                            current_key,
                            "orchard",
                            tonic::Status::not_found("block data missing from db (orchard)"),
                            true,
                        ) {
                            Some(pair) => pair,
                            None => return,
                        };
                        Some(val)
                    } else {
                        None
                    };

                let (_commitment_key, mut raw_commitment_tree_bytes) = match cursor_set_key_or_send(
                    &sender,
                    &commitment_tree_cursor,
                    current_key,
                    "commitment_tree_data",
                    tonic::Status::not_found("block data missing from db (commitment_tree_data)"),
                    true,
                ) {
                    Some(pair) => pair,
                    None => return,
                };

                let mut blocks_streamed_in_transaction: usize = 0;

                loop {
                    // ----- Decode and validate block header -----
                    let header: BlockHeaderData = match StoredEntryVar::from_bytes(raw_header_bytes)
                        .map_err(|error| format!("header decode error: {error}"))
                    {
                        Ok(entry) => *entry.inner(),
                        Err(message) => {
                            send_status(&sender, tonic::Status::internal(message));
                            return;
                        }
                    };

                    // Contiguous-height check: ensures cursor ordering and storage invariants are intact.
                    let current_height = header.context.height();
                    if current_height != expected_height {
                        send_status(
                            &sender,
                            tonic::Status::internal(format!(
                                "missing height or out-of-order headers: expected {}, got {}",
                                expected_height.0, current_height.0
                            )),
                        );
                        return;
                    }

                    // ----- Ensure the block is validated (on-demand) -----
                    // We are in a blocking task; call validate_block_blocking directly but only when needed.
                    if !zaino_db.is_validated(current_height.into()) {
                        // header.context.hash() is the block hash we just read from DB; call validator.
                        let block_hash = *header.context.hash();

                        match zaino_db.validate_block_blocking(current_height, block_hash) {
                            Ok(()) => {
                                // validation succeeded and mark_validated has been called inside the validator.
                            }
                            Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                                // missing data that was expected: emit DataUnavailable -> translate to not_found
                                send_status(
                                    &sender,
                                    tonic::Status::internal(format!(
                                        "block data unavailable during validation at height {}",
                                        current_height.0
                                    )),
                                );
                                return;
                            }
                            Err(e) => {
                                send_status(
                                    &sender,
                                    tonic::Status::internal(format!(
                                        "validation failed for height {}: {e:?}",
                                        current_height.0
                                    )),
                                );
                                return;
                            }
                        }
                    }

                    // ----- Decode txids and optional pool data -----
                    let txids_stored_entry_var =
                        match StoredEntryVar::<TxidList>::from_bytes(raw_txids_bytes)
                            .map_err(|error| format!("txids decode error: {error}"))
                        {
                            Ok(entry) => entry,
                            Err(message) => {
                                send_status(&sender, tonic::Status::internal(message));
                                return;
                            }
                        };
                    let txids = txids_stored_entry_var.inner().txids();

                    // Each pool database stores a per-height vector aligned to the txids list:
                    // one entry per transaction index (typically `Option<T>` per tx).
                    let transparent_entries: Option<StoredEntryVar<TransparentTxList>> =
                        if let Some(raw) = raw_transparent_bytes {
                            match StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                                .map_err(|error| format!("transparent decode error: {error}"))
                            {
                                Ok(entry) => Some(entry),
                                Err(message) => {
                                    send_status(&sender, tonic::Status::internal(message));
                                    return;
                                }
                            }
                        } else {
                            None
                        };

                    let sapling_entries: Option<StoredEntryVar<SaplingTxList>> =
                        if let Some(raw) = raw_sapling_bytes {
                            match StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                                .map_err(|error| format!("sapling decode error: {error}"))
                            {
                                Ok(entry) => Some(entry),
                                Err(message) => {
                                    send_status(&sender, tonic::Status::internal(message));
                                    return;
                                }
                            }
                        } else {
                            None
                        };

                    let orchard_entries: Option<StoredEntryVar<OrchardTxList>> =
                        if let Some(raw) = raw_orchard_bytes {
                            match StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                                .map_err(|error| format!("orchard decode error: {error}"))
                            {
                                Ok(entry) => Some(entry),
                                Err(message) => {
                                    send_status(&sender, tonic::Status::internal(message));
                                    return;
                                }
                            }
                        } else {
                            None
                        };

                    let transparent = match transparent_entries.as_ref() {
                        Some(entry) => entry.inner().tx(),
                        None => &[],
                    };
                    let sapling = match sapling_entries.as_ref() {
                        Some(entry) => entry.inner().tx(),
                        None => &[],
                    };
                    let orchard = match orchard_entries.as_ref() {
                        Some(entry) => entry.inner().tx(),
                        None => &[],
                    };

                    // Invariant: if a pool is requested, its per-height vector length must match txids.
                    if pool_types.includes_transparent() && transparent.len() != txids.len() {
                        send_status(
                        &sender,
                        tonic::Status::internal(format!(
                            "transparent list length mismatch at height {}: txids={}, transparent={}",
                            current_height.0,
                            txids.len(),
                            transparent.len(),
                        )),
                    );
                        return;
                    }
                    if pool_types.includes_sapling() && sapling.len() != txids.len() {
                        send_status(
                            &sender,
                            tonic::Status::internal(format!(
                                "sapling list length mismatch at height {}: txids={}, sapling={}",
                                current_height.0,
                                txids.len(),
                                sapling.len(),
                            )),
                        );
                        return;
                    }
                    if pool_types.includes_orchard() && orchard.len() != txids.len() {
                        send_status(
                            &sender,
                            tonic::Status::internal(format!(
                                "orchard list length mismatch at height {}: txids={}, orchard={}",
                                current_height.0,
                                txids.len(),
                                orchard.len(),
                            )),
                        );
                        return;
                    }

                    // ----- Build CompactTx list -----
                    //
                    // `CompactTx.index` is the original transaction index within the block.
                    // This implementation omits transactions that contain no elements in any requested pool type,
                    // which means:
                    // - `vtx.len()` may be smaller than the number of txids in the block, and
                    // - indices in `vtx` may be non-contiguous.
                    // Consumers must interpret `CompactTx.index` as authoritative.
                    //
                    // TODO: Re-evaluate whether omitting "empty-for-filter" transactions is the desired API behaviour.
                    //       Some clients may expect a position-preserving representation (one entry per txid), even if
                    //       the per-pool fields are empty for a given filter.
                    let mut vtx: Vec<zaino_proto::proto::compact_formats::CompactTx> =
                        Vec::with_capacity(txids.len());

                    for (i, txid) in txids.iter().enumerate() {
                        let spends = sapling
                            .get(i)
                            .and_then(|opt| opt.as_ref())
                            .map(|s| {
                                s.spends()
                                    .iter()
                                    .map(|sp| sp.into_compact())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();

                        let outputs = sapling
                            .get(i)
                            .and_then(|opt| opt.as_ref())
                            .map(|s| {
                                s.outputs()
                                    .iter()
                                    .map(|o| o.into_compact())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();

                        let actions = orchard
                            .get(i)
                            .and_then(|opt| opt.as_ref())
                            .map(|o| {
                                o.actions()
                                    .iter()
                                    .map(|a| a.into_compact())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();

                        let (vin, vout) = transparent
                            .get(i)
                            .and_then(|opt| opt.as_ref())
                            .map(|t| (t.compact_vin(), t.compact_vout()))
                            .unwrap_or_default();

                        // Omit transactions that have no elements in any requested pool type.
                        //
                        // Note that omission produces a sparse `vtx` (by original transaction index). Clients must use
                        // `CompactTx.index` rather than assuming contiguous ordering.
                        //
                        // TODO: Re-evaluate whether omission is the desired API behaviour for all consumers.
                        if spends.is_empty()
                            && outputs.is_empty()
                            && actions.is_empty()
                            && vin.is_empty()
                            && vout.is_empty()
                        {
                            continue;
                        }

                        vtx.push(zaino_proto::proto::compact_formats::CompactTx {
                            index: i as u64,
                            txid: txid.0.to_vec(),
                            fee: 0,
                            spends,
                            outputs,
                            actions,
                            vin,
                            vout,
                        });
                    }

                    // ----- Decode commitment tree data and construct block -----
                    let commitment_tree_data: CommitmentTreeData =
                        match StoredEntryFixed::from_bytes(raw_commitment_tree_bytes)
                            .map_err(|error| format!("commitment_tree decode error: {error}"))
                        {
                            Ok(entry) => *entry.inner(),
                            Err(message) => {
                                send_status(&sender, tonic::Status::internal(message));
                                return;
                            }
                        };

                    let chain_metadata = zaino_proto::proto::compact_formats::ChainMetadata {
                        sapling_commitment_tree_size: commitment_tree_data.sizes().sapling(),
                        orchard_commitment_tree_size: commitment_tree_data.sizes().orchard(),
                    };

                    let compact_block = zaino_proto::proto::compact_formats::CompactBlock {
                        proto_version: 4,
                        height: header.context.height().0 as u64,
                        hash: header.context.hash().0.to_vec(),
                        prev_hash: header.context.parent_hash().0.to_vec(),
                        // NOTE: `time()` is stored in the DB as a wider integer; this cast assumes it is
                        // always representable in `u32` for the protobuf.
                        time: header.data().time() as u32,
                        header: Vec::new(),
                        vtx,
                        chain_metadata: Some(chain_metadata),
                    };

                    // Send the block downstream; if the receiver is gone, stop immediately.
                    if sender.blocking_send(Ok(compact_block)).is_err() {
                        return;
                    }

                    // If we just emitted the inclusive end height, stop without stepping cursors further.
                    if current_height == validated_end_height {
                        return;
                    }

                    blocks_streamed_in_transaction += 1;

                    // Compute the next expected height (used both for contiguity checking and chunk re-seek).
                    let next_expected_height = if is_ascending {
                        match expected_height.0.checked_add(1) {
                            Some(value) => Height(value),
                            None => {
                                send_status(
                                    &sender,
                                    tonic::Status::internal(
                                        "expected_height overflow while iterating ascending"
                                            .to_string(),
                                    ),
                                );
                                return;
                            }
                        }
                    } else {
                        match expected_height.0.checked_sub(1) {
                            Some(value) => Height(value),
                            None => {
                                send_status(
                                    &sender,
                                    tonic::Status::internal(
                                        "expected_height underflow while iterating descending"
                                            .to_string(),
                                    ),
                                );
                                return;
                            }
                        }
                    };

                    // Chunk boundary: drop the current read transaction after N blocks and re-seek in a new
                    // transaction on the next loop iteration. This avoids a single long-lived snapshot.
                    if blocks_streamed_in_transaction >= BLOCKS_PER_READ_TRANSACTION {
                        match next_expected_height.to_bytes() {
                            Ok(bytes) => {
                                next_start_key_bytes = bytes;
                                expected_height = next_expected_height;
                                break;
                            }
                            Err(error) => {
                                send_status(
                                    &sender,
                                    tonic::Status::internal(format!(
                                        "height to_bytes failed at chunk boundary: {error}"
                                    )),
                                );
                                return;
                            }
                        }
                    }

                    // Advance all cursors in lockstep. Headers drives the next key; all others must match it.
                    let next_headers = match headers_step_or_send(&sender, &headers_cursor, step_op)
                    {
                        Ok(value) => value,
                        Err(()) => return,
                    };

                    let (next_key, next_header_val) = match next_headers {
                        Some(pair) => pair,
                        None => {
                            // Headers ended early; if we have not reached the requested end height, the
                            // database no longer satisfies the contiguous-height invariant for this range.
                            if current_height != validated_end_height {
                                send_status(
                                    &sender,
                                    tonic::Status::internal(format!(
                                    "headers cursor ended early at height {}; expected to reach {}",
                                    current_height.0, validated_end_height.0
                                )),
                                );
                            }
                            return;
                        }
                    };

                    let next_txids_val = match cursor_step_expect_key_or_send(
                        &sender,
                        &txids_cursor,
                        step_op,
                        next_key,
                        "txids",
                        tonic::Status::not_found("block data missing from db (txids)"),
                    ) {
                        Some(val) => val,
                        None => return,
                    };

                    let next_transparent_val: Option<&[u8]> = if let Some(cursor) =
                        transparent_cursor.as_ref()
                    {
                        match cursor_step_expect_key_or_send(
                            &sender,
                            cursor,
                            step_op,
                            next_key,
                            "transparent",
                            tonic::Status::not_found("block data missing from db (transparent)"),
                        ) {
                            Some(val) => Some(val),
                            None => return,
                        }
                    } else {
                        None
                    };

                    let next_sapling_val: Option<&[u8]> =
                        if let Some(cursor) = sapling_cursor.as_ref() {
                            match cursor_step_expect_key_or_send(
                                &sender,
                                cursor,
                                step_op,
                                next_key,
                                "sapling",
                                tonic::Status::not_found("block data missing from db (sapling)"),
                            ) {
                                Some(val) => Some(val),
                                None => return,
                            }
                        } else {
                            None
                        };

                    let next_orchard_val: Option<&[u8]> =
                        if let Some(cursor) = orchard_cursor.as_ref() {
                            match cursor_step_expect_key_or_send(
                                &sender,
                                cursor,
                                step_op,
                                next_key,
                                "orchard",
                                tonic::Status::not_found("block data missing from db (orchard)"),
                            ) {
                                Some(val) => Some(val),
                                None => return,
                            }
                        } else {
                            None
                        };

                    let next_commitment_tree_val = match cursor_step_expect_key_or_send(
                        &sender,
                        &commitment_tree_cursor,
                        step_op,
                        next_key,
                        "commitment_tree_data",
                        tonic::Status::not_found(
                            "block data missing from db (commitment_tree_data)",
                        ),
                    ) {
                        Some(val) => val,
                        None => return,
                    };

                    raw_header_bytes = next_header_val;
                    raw_txids_bytes = next_txids_val;
                    raw_transparent_bytes = next_transparent_val;
                    raw_sapling_bytes = next_sapling_val;
                    raw_orchard_bytes = next_orchard_val;
                    raw_commitment_tree_bytes = next_commitment_tree_val;

                    expected_height = next_expected_height;
                }
            }
        });

        Ok(CompactBlockStream::new(receiver))
    }

    // *** Internal DB methods ***
}
