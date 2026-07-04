//! FinalisedState::V1 core block indexing functionality.

use super::*;

/// [`BlockCoreExt`] capability implementation for [`DbV1`].
///
/// Provides access to block headers, txid lists, and transaction location mapping.
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
        self.read_row_at_height(self.headers, "header", height)
            .await?
            .ok_or_else(|| {
                FinalisedStateError::DataUnavailable("header data missing from db".into())
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
        self.scan_rows(self.headers, "header", start, end).await
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
            let transaction_versioned_len = TransactionHash::latest_versioned_len()?;
            let offset = cursor.position() + (idx as u64) * transaction_versioned_len as u64;
            cursor.set_position(offset);

            // Read [0] Txid Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Then read 32 bytes for the txid
            let transaction_encoded_len = TransactionHash::latest_encoded_len()?;
            if transaction_encoded_len != 32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "TransactionHash latest encoded length must be 32 bytes, got {}",
                        transaction_encoded_len
                    ),
                ))?;
            }

            let mut txid_bytes = [0u8; 32];
            cursor
                .read_exact(&mut txid_bytes)
                .map_err(|e| FinalisedStateError::Custom(format!("txid read error: {e}")))?;

            Ok(TransactionHash::from(txid_bytes))
        })
    }

    /// Fetch block txids by height.
    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        self.read_row_at_height(self.txids, "txids", height)
            .await?
            .ok_or_else(|| FinalisedStateError::DataUnavailable("txid data missing from db".into()))
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
        self.scan_rows(self.txids, "txids", start, end).await
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
