//! FinalisedState::V1 DB validation and varification functionality.
//!
//! The finalised-state database supports **incremental, concurrency-safe validation** of blocks that
//! have already been written to LMDB.
//!
//! Validation is tracked using two structures:
//!
//! - `validated_tip` (atomic u32): every height `<= validated_tip` is known-good (contiguous prefix).
//! - `validated_set` (`DashSet<u32>`): a sparse set of individually validated heights `> validated_tip`
//!   (i.e., “holes” validated out-of-order).
//!
//! This scheme provides:
//! - O(1) fast-path for the common case (`height <= validated_tip`),
//! - O(1) expected membership tests above the tip,
//! - and an efficient “coalescing” step that advances `validated_tip` when gaps are filled.
//!
//! IMPORTANT:
//! - Validation here is *structural / integrity* validation of stored records plus basic chain
//!   continuity checks (parent hash, header merkle root vs txids).
//! - It is intentionally “lightweight” and does **not** attempt full consensus verification.
//! - NOTE / TODO: It is planned to add basic shielded tx data validation using the "block_commitments"
//!   field in [`BlockData`] however this is currently unimplemented.

use super::*;

impl DbV1 {
    /// Return `true` if `height` is already known-good.
    ///
    /// Semantics:
    /// - `height <= validated_tip` is always validated (contiguous prefix).
    /// - For `height > validated_tip`, membership is tracked in `validated_set`.
    ///
    /// Performance:
    /// - O(1) in the fast-path (`height <= validated_tip`).
    /// - O(1) expected for DashSet membership checks when `height > validated_tip`.
    ///
    /// Concurrency:
    /// - `validated_tip` is read with `Acquire` so subsequent reads of dependent state in the same
    ///   thread are not reordered before the tip read.
    pub(super) fn is_validated(&self, h: u32) -> bool {
        let tip = self.validated_tip.load(Ordering::Acquire);
        h <= tip || self.validated_set.contains(&h)
    }

    /// Mark `height` as validated and coalesce contiguous ranges into `validated_tip`.
    ///
    /// This method maintains the invariant:
    /// - After completion, all heights `<= validated_tip` are validated.
    /// - All validated heights `> validated_tip` remain represented in `validated_set`.
    ///
    /// Algorithm:
    /// 1. If `height == validated_tip + 1`, attempt to atomically advance `validated_tip`.
    /// 2. If that succeeds, repeatedly consume `validated_tip + 1` from `validated_set` and advance
    ///    `validated_tip` until the next height is not present.
    /// 3. If `height > validated_tip + 1`, record it as an out-of-order validated “hole” in
    ///    `validated_set`.
    /// 4. If `height <= validated_tip`, it is already covered by the contiguous prefix; no action.
    ///
    /// Concurrency:
    /// - Uses CAS to ensure only one thread advances `validated_tip` at a time.
    /// - Stores after successful coalescing use `Release` so other threads observing the new tip do not
    ///   see older state re-ordered after the tip update.
    ///
    /// NOTE:
    /// - This function is intentionally tolerant of races: redundant inserts / removals are benign.
    pub(super) fn mark_validated(&self, h: u32) {
        let mut next = h;
        loop {
            let tip = self.validated_tip.load(Ordering::Acquire);

            // Fast-path: extend the tip directly?
            if next == tip + 1 {
                // Try to claim the new tip.
                if self
                    .validated_tip
                    .compare_exchange(tip, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // Successfully advanced; now look for further consecutive heights
                    // already in the DashSet.
                    next += 1;
                    while self.validated_set.remove(&next).is_some() {
                        self.validated_tip.store(next, Ordering::Release);
                        next += 1;
                    }
                    break;
                }
                // CAS failed: someone else updated the tip – retry loop.
            } else if next > tip {
                // Out-of-order hole: just remember it and exit.
                self.validated_set.insert(next);
                break;
            } else {
                // Already below tip – nothing to do.
                break;
            }
        }
    }

    /// Lightweight per-block validation.
    ///
    /// This validates the internal consistency of the LMDB-backed records for the specified
    /// `(height, hash)` pair and marks the height as validated on success.
    ///
    /// Validations performed:
    /// - Per-height tables: checksum + deserialization integrity for:
    ///   - `headers` (BlockHeaderData)
    ///   - `txids` (TxidList)
    ///   - `transparent` (TransparentTxList)
    ///   - `sapling` (SaplingTxList)
    ///   - `orchard` (OrchardTxList)
    ///   - `commitment_tree_data` (CommitmentTreeData; fixed entry)
    /// - Hash→height mapping:
    ///   - checksum integrity under `hash_key`
    ///   - mapped height equals the requested `height`
    /// - Chain continuity:
    ///   - for `height > 1`, the block header `parent_hash` equals the stored hash at `height - 1`
    /// - Header merkle root:
    ///   - merkle root computed from `txids` matches the header’s merkle root
    /// - Transparent indices / histories:
    ///   - each non-coinbase transparent input must have a `spent` record pointing at this tx
    ///   - each transparent output must have an addrhist mined record
    ///   - each non-coinbase transparent input must have an addrhist input record
    ///
    /// Fast-path:
    /// - If `height` is already known validated (`is_validated`), this is a no-op.
    ///
    /// Error semantics:
    /// - Returns `FinalisedStateError::InvalidBlock { .. }` when any integrity/continuity check fails.
    /// - Returns LMDB errors for underlying storage failures (e.g., missing keys), which are then
    ///   typically mapped by callers into `DataUnavailable` where appropriate.
    ///
    /// WARNING:
    /// - This is a blocking function and **MUST** be called from a blocking context
    ///   (`tokio::task::block_in_place` or `spawn_blocking`).
    pub(super) fn validate_block_blocking(
        &self,
        height: Height,
        hash: BlockHash,
    ) -> Result<(), FinalisedStateError> {
        if self.is_validated(height.into()) {
            return Ok(());
        }

        let height_key = height
            .to_bytes()
            .map_err(|e| FinalisedStateError::Custom(format!("height serialize: {e}")))?;
        let hash_key = hash
            .to_bytes()
            .map_err(|e| FinalisedStateError::Custom(format!("hash serialize: {e}")))?;

        // Helper to fabricate the error.
        let fail = |reason: &str| FinalisedStateError::InvalidBlock {
            height: height.into(),
            hash,
            reason: reason.to_owned(),
        };

        let ro = self.env.begin_ro_txn()?;

        // *** header ***
        let header_entry = {
            let raw = ro
                .get(self.headers, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                .map_err(|e| fail(&format!("header corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("header checksum mismatch"));
            }
            entry
        };

        // *** txids ***
        let txid_list_entry = {
            let raw = ro
                .get(self.txids, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| fail(&format!("txids corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("txids checksum mismatch"));
            }
            entry
        };

        // *** transparent ***
        let transparent_tx_list = {
            let raw = ro.get(self.transparent, &height_key)?;
            let entry = StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("transparent corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("transparent checksum mismatch"));
            }
            entry
        };

        // *** sapling ***
        {
            let raw = ro
                .get(self.sapling, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("sapling corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("sapling checksum mismatch"));
            }
        }

        // *** orchard ***
        {
            let raw = ro
                .get(self.orchard, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("orchard corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("orchard checksum mismatch"));
            }
        }

        // *** commitment_tree_data (fixed) ***
        {
            let raw = ro
                .get(self.commitment_tree_data, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryFixed::<CommitmentTreeData>::from_bytes(raw)
                .map_err(|e| fail(&format!("commitment_tree corrupt bytes: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("commitment_tree checksum mismatch"));
            }
        }

        // *** hash→height mapping ***
        {
            let raw = ro
                .get(self.heights, &hash_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryFixed::<Height>::from_bytes(raw)
                .map_err(|e| fail(&format!("hash -> height corrupt bytes: {e}")))?;
            if !entry.verify(&hash_key) {
                return Err(fail("hash -> height checksum mismatch"));
            }
            if entry.item != height {
                return Err(fail("hash -> height mapping mismatch"));
            }
        }

        // *** Parent block hash validation (chain continuity) ***
        if height.0 > 1 {
            let parent_block_hash = {
                let parent_block_height = Height::try_from(height.0.saturating_sub(1))
                    .map_err(|e| fail(&format!("invalid parent height: {e}")))?;
                let parent_block_height_key = parent_block_height
                    .to_bytes()
                    .map_err(|e| fail(&format!("parent height serialize: {e}")))?;
                let raw = ro
                    .get(self.headers, &parent_block_height_key)
                    .map_err(FinalisedStateError::LmdbError)?;
                let entry = StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                    .map_err(|e| fail(&format!("parent header corrupt data: {e}")))?;

                *entry.inner().context.hash()
            };

            let check_hash = header_entry.inner().context.parent_hash();

            if &parent_block_hash != check_hash {
                return Err(fail("parent hash mismatch"));
            }
        }

        // *** Merkle root / Txid validation ***
        let txids: Vec<[u8; 32]> = txid_list_entry
            .inner()
            .txids()
            .iter()
            .map(|h| h.0)
            .collect();

        let header_merkle_root = header_entry.inner().data().merkle_root();

        let check_root = Self::calculate_block_merkle_root(&txids);

        if &check_root != header_merkle_root {
            return Err(fail("merkle root mismatch"));
        }

        // *** spent  ***
        let validate_spent_index = {
            let metadata_key = b"metadata";

            let raw = ro
                .get(self.metadata, metadata_key)
                .map_err(FinalisedStateError::LmdbError)?;

            let entry = StoredEntryFixed::<DbMetadata>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("metadata corrupt data: {e}")))?;

            if !entry.verify(metadata_key) {
                return Err(FinalisedStateError::Custom(
                    "metadata checksum mismatch".to_string(),
                ));
            }

            entry.inner().version
                >= DbVersion {
                    major: 1,
                    minor: 2,
                    patch: 0,
                }
        };
        if validate_spent_index {
            let tx_list = transparent_tx_list.inner().tx();

            for (tx_index, tx_opt) in tx_list.iter().enumerate() {
                let tx_index = tx_index as u16;
                let txid_index = TxLocation::new(height.0, tx_index);

                let Some(tx) = tx_opt else { continue };

                // Inputs: check spent + addrhist input record
                for input in tx.inputs().iter() {
                    // Continue if coinbase.
                    if input.is_null_prevout() {
                        continue;
                    }

                    // Check spent record
                    let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                    let outpoint_bytes = outpoint.to_bytes()?;
                    let val = ro.get(self.spent, &outpoint_bytes).map_err(|_| {
                        fail(&format!("missing spent index for outpoint {outpoint:?}"))
                    })?;
                    let entry = StoredEntryFixed::<TxLocation>::from_bytes(val)
                        .map_err(|e| fail(&format!("corrupt spent entry: {e}")))?;
                    if !entry.verify(&outpoint_bytes) {
                        return Err(fail("spent entry checksum mismatch"));
                    }
                    if entry.inner() != &txid_index {
                        return Err(fail("spent entry has wrong TxLocation"));
                    }
                }
            }
        }

        // *** addrhist validation ***
        #[cfg(feature = "transparent_address_history_experimental")]
        {
            let tx_list = transparent_tx_list.inner().tx();

            for (tx_index, tx_opt) in tx_list.iter().enumerate() {
                let tx_index = tx_index as u16;
                let txid_index = TxLocation::new(height.0, tx_index);

                let Some(tx) = tx_opt else { continue };

                // Outputs: check addrhist mined record
                for (vout, output) in tx.outputs().iter().enumerate() {
                    let addr_bytes =
                        AddrScript::new(*output.script_hash(), output.script_type()).to_bytes()?;
                    let rec_bytes = self.addr_hist_records_by_addr_and_index_in_txn(
                        &ro,
                        &addr_bytes,
                        txid_index,
                    )?;

                    let matched = rec_bytes.iter().any(|val| {
                        // avoid deserialization: check IS_MINED + correct vout
                        // - [0] StoredEntry tag
                        // - [1] record tag
                        // - [2..=5] height
                        // - [6..=7] tx_index
                        // - [8..=9] vout
                        // - [10] flags
                        // - [11..=18] value
                        // - [19..=50] checksum

                        let flags = val[10];
                        let vout_rec = u16::from_be_bytes([val[8], val[9]]);
                        (flags & AddrEventBytes::FLAG_MINED) != 0 && vout_rec as usize == vout
                    });

                    if !matched {
                        return Err(fail("missing addrhist mined output record"));
                    }
                }

                // Inputs: check spent + addrhist input record
                for (input_index, input) in tx.inputs().iter().enumerate() {
                    // Continue if coinbase.
                    if input.is_null_prevout() {
                        continue;
                    }

                    let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                    // Check addrhist input record
                    let prev_output = self.get_previous_output_blocking(outpoint)?;
                    let addr_bytes =
                        AddrScript::new(*prev_output.script_hash(), prev_output.script_type())
                            .to_bytes()?;
                    let rec_bytes = self.addr_hist_records_by_addr_and_index_in_txn(
                        &ro,
                        &addr_bytes,
                        txid_index,
                    )?;

                    let matched = rec_bytes.iter().any(|val| {
                        // avoid deserialization: check IS_INPUT + correct vout
                        // - [0] StoredEntry tag
                        // - [1] record tag
                        // - [2..=5] height
                        // - [6..=7] tx_index
                        // - [8..=9] vout
                        // - [10] flags
                        // - [11..=18] value
                        // - [19..=50] checksum

                        let flags = val[10];
                        let stored_vout = u16::from_be_bytes([val[8], val[9]]);

                        (flags & AddrEventBytes::FLAG_IS_INPUT) != 0
                            && stored_vout as usize == input_index
                    });

                    if !matched {
                        return Err(fail("missing addrhist input record"));
                    }
                }
            }
        }

        self.mark_validated(height.into());
        Ok(())
    }

    /// Double-SHA-256 (SHA256d), as used by Bitcoin/Zcash headers and merkle nodes.
    ///
    /// Input and output are raw bytes (no endianness conversions are performed here).
    fn sha256d(data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, data); // first pass
        let first = hasher.finalize_reset();
        Digest::update(&mut hasher, first); // second pass
        let second = hasher.finalize();

        let mut out = [0u8; 32];
        out.copy_from_slice(&second);
        out
    }

    /// Compute the merkle root of a non-empty slice of 32-byte transaction IDs.
    ///
    /// Requirements:
    /// - `txids` must be in block order.
    /// - `txids` must already be in the internal byte order (little endian) expected by the header merkle root
    ///   comparison performed by this module (no byte order transforms are applied here).
    ///
    /// Behavior:
    /// - Duplicates the final element when the layer width is odd, matching Bitcoin/Zcash merkle rules.
    /// - Uses SHA256d over 64-byte concatenated pairs at each layer.
    pub(super) fn calculate_block_merkle_root(txids: &[[u8; 32]]) -> [u8; 32] {
        assert!(
            !txids.is_empty(),
            "block must contain at least the coinbase"
        );
        let mut layer: Vec<[u8; 32]> = txids.to_vec();

        // Iterate until we have reduced to one hash.
        while layer.len() > 1 {
            let mut next = Vec::with_capacity(layer.len().div_ceil(2));

            // Combine pairs (duplicate the last when the count is odd).
            for chunk in layer.chunks(2) {
                let left = &chunk[0];
                let right = if chunk.len() == 2 {
                    &chunk[1]
                } else {
                    &chunk[0]
                };

                // Concatenate left‖right and hash twice.
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(left);
                buf[32..].copy_from_slice(right);
                next.push(Self::sha256d(&buf));
            }

            layer = next;
        }

        layer[0]
    }

    /// Ensure `height` is validated.  If it's already validated this is a cheap O(1) check.
    /// Otherwise this will perform blocking validation (`validate_block_blocking`) and mark
    /// the height validated on success.
    ///
    /// This is the canonical, async-friendly entrypoint you should call from async code.
    pub(crate) async fn validate_height(
        &self,
        height: Height,
        hash: BlockHash,
    ) -> Result<(), FinalisedStateError> {
        // Cheap fast-path first, no blocking.
        if self.is_validated(height.into()) {
            return Ok(());
        }

        // Run blocking validation in a blocking context.
        // Using block_in_place keeps the per-call semantics similar to other callers.
        tokio::task::block_in_place(|| self.validate_block_blocking(height, hash))
    }

    /// Validate a contiguous inclusive range of block heights `[start, end]`.
    ///
    /// This method is optimized to skip heights already known validated via `validated_tip` /
    /// `validated_set`.
    ///
    /// Semantics:
    /// - Accepts either ordering of `start` and `end`.
    /// - Validates the inclusive set `{min(start,end) ..= max(start,end)}` in ascending order.
    /// - If the entire normalized range is already validated, returns `(start, end)` without
    ///   touching LMDB (preserves the caller's original ordering).
    /// - Otherwise, validates each missing height in ascending order using `validate_block_blocking`.
    ///
    /// WARNING:
    /// - This uses `tokio::task::block_in_place` internally and performs LMDB reads; callers should
    ///   avoid invoking it from latency-sensitive async paths unless they explicitly intend to
    ///   validate on-demand.
    pub(super) async fn validate_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<(Height, Height), FinalisedStateError> {
        // Normalize the range for validation, but preserve `(start, end)` ordering in the return.
        let (range_start, range_end) = if start.0 <= end.0 {
            (start, end)
        } else {
            (end, start)
        };

        let tip = self.validated_tip.load(Ordering::Acquire);
        let mut h = std::cmp::max(range_start.0, tip);

        if h > range_end.0 {
            return Ok((start, end));
        }

        tokio::task::block_in_place(|| {
            while h <= range_end.0 {
                if self.is_validated(h) {
                    h += 1;
                    continue;
                }

                let height = Height(h);
                let height_bytes = height.to_bytes()?;
                let ro = self.env.begin_ro_txn()?;
                let bytes = ro.get(self.headers, &height_bytes).map_err(|e| {
                    if e == lmdb::Error::NotFound {
                        FinalisedStateError::Custom("height not found in best chain".into())
                    } else {
                        FinalisedStateError::LmdbError(e)
                    }
                })?;

                let hash = StoredEntryVar::<BlockHeaderData>::deserialize(bytes)?
                    .inner()
                    .context
                    .index
                    .hash;

                match self.validate_block_blocking(height, hash) {
                    Ok(()) => {}
                    Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                        return Err(FinalisedStateError::DataUnavailable(
                            "block data unavailable".into(),
                        ));
                    }
                    Err(e) => return Err(e),
                }

                h += 1;
            }
            Ok::<_, FinalisedStateError>((start, end))
        })
    }

    /// Same as `resolve_hash_or_height`, **but guarantees the block is validated**.
    ///
    /// * If the block hasn’t been validated yet we do it on-demand
    /// * On success the block hright is returned; on any failure you get a
    ///   `FinalisedStateError`
    ///
    /// TODO: Remove HashOrHeight?
    pub(super) async fn resolve_validated_hash_or_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, FinalisedStateError> {
        let height = match hash_or_height {
            // Height lookup path.
            HashOrHeight::Height(z_height) => {
                let height = Height::try_from(z_height.0)
                    .map_err(|_| FinalisedStateError::Custom("height out of range".into()))?;

                // Check if height is below validated tip,
                // this avoids hash lookups for height based fetch under the valdated tip.
                if height.0 <= self.validated_tip.load(Ordering::Acquire) {
                    return Ok(height);
                }

                let hkey = height.to_bytes()?;

                tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.headers, &hkey).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            FinalisedStateError::DataUnavailable(
                                "height not found in best chain".into(),
                            )
                        } else {
                            FinalisedStateError::LmdbError(e)
                        }
                    })?;

                    let hash = StoredEntryVar::<BlockHeaderData>::deserialize(bytes)?
                        .inner()
                        .context
                        .index
                        .hash;

                    match self.validate_block_blocking(height, hash) {
                        Ok(()) => {}
                        Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                            return Err(FinalisedStateError::DataUnavailable(
                                "block data unavailable".into(),
                            ));
                        }
                        Err(e) => return Err(e),
                    }

                    Ok::<BlockHash, FinalisedStateError>(hash)
                })?;
                height
            }

            // Hash lookup path.
            HashOrHeight::Hash(z_hash) => {
                let height = self.resolve_hash_or_height(hash_or_height).await?;
                let hash = BlockHash::from(z_hash);
                tokio::task::block_in_place(|| {
                    match self.validate_block_blocking(height, hash) {
                        Ok(()) => {}
                        Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                            return Err(FinalisedStateError::DataUnavailable(
                                "block data unavailable".into(),
                            ));
                        }
                        Err(e) => return Err(e),
                    }

                    Ok::<Height, FinalisedStateError>(height)
                })?;
                height
            }
        };

        Ok(height)
    }

    /// Resolve a `HashOrHeight` to the block height stored on disk.
    ///
    /// * Height  ->  returned unchanged (zero cost).
    /// * Hash ->  lookup in `hashes` db.
    ///
    /// TODO: Remove HashOrHeight?
    async fn resolve_hash_or_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, FinalisedStateError> {
        match hash_or_height {
            // Fast path: we already have the hash.
            HashOrHeight::Height(z_height) => Ok(Height::try_from(z_height.0)
                .map_err(|_| FinalisedStateError::DataUnavailable("height out of range".into()))?),

            // Height lookup path.
            HashOrHeight::Hash(z_hash) => {
                let hash = BlockHash::from(z_hash.0);
                let hkey = hash.to_bytes()?;

                let height: Height = tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.heights, &hkey).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            FinalisedStateError::DataUnavailable(
                                "height not found in best chain".into(),
                            )
                        } else {
                            FinalisedStateError::LmdbError(e)
                        }
                    })?;

                    let entry = *StoredEntryFixed::<Height>::deserialize(bytes)?.inner();
                    Ok::<Height, FinalisedStateError>(entry)
                })?;

                Ok(height)
            }
        }
    }

    /// Ensure the `metadata` table contains **exactly** our `DB_SCHEMA_V1`.
    ///
    /// * Brand-new DB → insert the entry.
    /// * Existing DB  → verify checksum, version, and schema hash.
    pub(super) async fn check_schema_version(&self) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            match txn.get(self.metadata, b"metadata") {
                // ***** Existing DB *****
                Ok(raw_bytes) => {
                    let stored: StoredEntryFixed<DbMetadata> =
                        StoredEntryFixed::from_bytes(raw_bytes).map_err(|e| {
                            FinalisedStateError::Custom(format!("corrupt metadata CBOR: {e}"))
                        })?;
                    if !stored.verify(b"metadata") {
                        return Err(FinalisedStateError::Custom(
                            "metadata checksum mismatch – DB corruption suspected".into(),
                        ));
                    }

                    let meta = stored.item;

                    // Error if major version differs
                    if meta.version.major != DB_VERSION_V1.major {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported schema major version {} (expected {})",
                            meta.version.major, DB_VERSION_V1.major
                        )));
                    }

                    // Warn if schema hash mismatches
                    // NOTE: There could be a schema mismatch at launch during minor migrations,
                    //       so we do not return an error here. Maybe we can improve this?
                    if meta.schema_hash != DB_SCHEMA_V1_HASH {
                        warn!(
                            "schema hash mismatch: db_schema_v1.txt has likely changed \
                         without bumping version; expected 0x{:02x?}, found 0x{:02x?}",
                            &DB_SCHEMA_V1_HASH[..4],
                            &meta.schema_hash[..4],
                        );
                    }
                }

                // ***** Fresh DB (key not found) *****
                Err(lmdb::Error::NotFound) => {
                    let entry = StoredEntryFixed::new(
                        b"metadata",
                        DbMetadata {
                            version: DB_VERSION_V1,
                            schema_hash: DB_SCHEMA_V1_HASH,
                            // Fresh database, no migration required.
                            migration_status: MigrationStatus::Empty,
                        },
                    );
                    txn.put(
                        self.metadata,
                        b"metadata",
                        &entry.to_bytes()?,
                        WriteFlags::NO_OVERWRITE,
                    )?;
                }

                // ***** Any other LMDB error *****
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }

            txn.commit()?;
            Ok(())
        })
    }
}
