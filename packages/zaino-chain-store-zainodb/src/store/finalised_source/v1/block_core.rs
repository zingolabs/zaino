//! FinalisedState::V1 core block indexing functionality.

use super::*;

/// [`BlockCoreExt`] capability implementation for [`DbV1`].
///
/// Provides access to block headers, txid lists, and transaction location mapping.
impl BlockCoreExt for DbV1 {
    #[cfg(test)]
    async fn get_block_header(&self, height: Height) -> Result<BlockHeaderData, StoreError> {
        self.get_block_header_data(height).await
    }

    async fn get_txid(&self, tx_location: TxLocation) -> Result<TransactionHash, StoreError> {
        self.get_txid(tx_location).await
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, StoreError> {
        self.get_tx_location(txid).await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Fetch block header data by height.
    pub(super) async fn get_block_header_data(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, StoreError> {
        self.read_row_at_height::<BlockHeaderData<AbsoluteChainWork>>(
            self.headers,
            "header",
            height,
        )
        .await?
        .map(|header| header.map_chainwork(Some))
        .ok_or_else(|| StoreError::DataUnavailable("header data missing from db".into()))
    }

    /// Fetch the txid bytes for a given TxLocation.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_txid(&self, tx_location: TxLocation) -> Result<TransactionHash, StoreError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            use std::io::Cursor;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| StoreError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Read CompactSize: number of txids
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| StoreError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                // Missing data, not a malformed row: the block is shorter than
                // the index asked about. `DataUnavailable` is what says so, and
                // it is what lets a caller distinguish "nothing at that
                // position" from "this row will not decode" — the port layer
                // answers `None` for the first and propagates the second.
                return Err(StoreError::DataUnavailable(
                    "tx_index out of range in txid list".to_string(),
                ));
            }

            // Each txid entry is 32 bytes, so skip the `idx` entries before it
            let offset = cursor.position() + (idx as u64) * TransactionHash::ENCODED_LEN as u64;
            cursor.set_position(offset);

            let mut txid_bytes = [0u8; TransactionHash::ENCODED_LEN];
            cursor
                .read_exact(&mut txid_bytes)
                .map_err(|e| StoreError::Custom(format!("txid read error: {e}")))?;

            Ok(TransactionHash::from(txid_bytes))
        })
    }

    // Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, StoreError> {
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
    ) -> Result<Option<TxLocation>, StoreError> {
        let ro = self.env.begin_ro_txn()?;

        // Reverse-index point lookup: `txid_location` maps a txid directly to its
        // `TxLocation`, replacing the former full scan of the height-keyed `txids` table.
        let key: [u8; 32] = (*txid).into();

        match ro.get(self.txid_location, &key) {
            Ok(stored_bytes) => {
                let location = TxLocation::from_bytes(stored_bytes)
                    .map_err(|e| StoreError::Custom(format!("corrupt txid_location entry: {e}")))?;
                Ok(Some(location))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(StoreError::LmdbError(e)),
        }
    }
}
