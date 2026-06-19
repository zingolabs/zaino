//! FinalisedState::V1 core block indexing functionality.

use super::*;

/// [`BlockCoreExt`] capability implementation for [`DbV1`].
///
/// Provides access to block headers, txid lists, and transaction location mapping.
#[async_trait]
impl BlockCoreExt for DbV1 {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        self.get_block_header_data(height).await
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        self.get_block_range_headers(start, end).await
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        self.get_block_txids(height).await
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        self.get_block_range_txids(start, end).await
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        self.get_txid(tx_location).await
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.get_tx_location(txid).await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Fetch block header data by height.
    pub(super) async fn get_block_header_data(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "header data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let entry = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?;

            Ok(*entry.inner())
        })
    }

    /// Fetches block headers for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    /// NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        if end.0 < start.0 {
            return Err(FinalisedStateError::Custom(
                "invalid block range: end < start".to_string(),
            ));
        }

        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.headers) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "header data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<BlockHeaderData>::from_bytes(&bytes)
                    .map(|e| *e.inner())
                    .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))
            })
            .collect()
    }

    /// Fetch the txid bytes for a given TxLocation.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    ///
    /// NOTE: This method currently ignores the txid version byte for efficiency.
    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            use std::io::Cursor;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Parse StoredEntryVar<TxidList>:

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            let _body_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Read [1] TxidList Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of txids
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| FinalisedStateError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in txid list".to_string(),
                ));
            }

            // Each txid entry is: [0] version tag + [1..32] txid

            // So we skip idx * 33 bytes to reach the start of the correct Hash
            let offset = cursor.position() + (idx as u64) * TransactionHash::VERSIONED_LEN as u64;
            cursor.set_position(offset);

            // Read [0] Txid Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Then read 32 bytes for the txid
            let mut txid_bytes = [0u8; TransactionHash::ENCODED_LEN];
            cursor
                .read_exact(&mut txid_bytes)
                .map_err(|e| FinalisedStateError::Custom(format!("txid read error: {e}")))?;

            Ok(TransactionHash::from(txid_bytes))
        })
    }

    /// Fetch block txids by height.
    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<TxidList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block txids for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    /// NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        if end.0 < start.0 {
            return Err(FinalisedStateError::Custom(
                "invalid block range: end < start".to_string(),
            ));
        }

        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.txids) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<TxidList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))
            })
            .collect()
    }

    // Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        if let Some(index) = tokio::task::block_in_place(|| self.find_txid_index_blocking(txid))? {
            Ok(Some(index))
        } else {
            Ok(None)
        }
    }

    // *** Internal DB methods ***

    /// Finds a TxLocation [block_height, tx_index] from a given txid.
    /// Used for Txid based lookup in transaction DBs.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    pub(super) fn find_txid_index_blocking(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        let ro = self.env.begin_ro_txn()?;

        // Reverse-index point lookup: `txid_location` maps a txid directly to its
        // `TxLocation`, replacing the former full scan of the height-keyed `txids` table.
        let key: [u8; 32] = (*txid).into();

        match ro.get(self.txid_location, &key) {
            Ok(stored_bytes) => {
                let entry =
                    StoredEntryFixed::<TxLocation>::from_bytes(stored_bytes).map_err(|e| {
                        FinalisedStateError::Custom(format!("corrupt txid_location entry: {e}"))
                    })?;
                if !entry.verify(key) {
                    return Err(FinalisedStateError::Custom(
                        "txid_location entry checksum mismatch".to_string(),
                    ));
                }
                Ok(Some(*entry.inner()))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(FinalisedStateError::LmdbError(e)),
        }
    }
}
