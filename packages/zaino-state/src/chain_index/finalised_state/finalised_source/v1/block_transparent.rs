//! FinalisedState::V1 transparent block indexing functionality.

use super::*;

/// [`BlockTransparentExt`] capability implementation for [`DbV1`].
///
/// Provides access to transparent compact transaction data at both per-transaction and per-block
/// granularity.
impl BlockTransparentExt for DbV1 {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        self.get_transparent(tx_location).await
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        self.get_block_transparent(height).await
    }

    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        self.get_block_range_transparent(start, end).await
    }

    async fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, FinalisedStateError> {
        tokio::task::block_in_place(|| self.get_previous_output_blocking(outpoint))
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Fetch the serialized TransparentCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            let _body_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Read [1] TransparentTxList Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of records
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| FinalisedStateError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in transparent tx data".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_transparent_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let option_start = cursor.position();

            // Peek at the 1-byte presence flag
            let mut presence = [0u8; 1];
            cursor.read_exact(&mut presence).map_err(|e| {
                FinalisedStateError::Custom(format!("failed to read Option tag: {e}"))
            })?;

            if presence[0] == 0 {
                return Ok(None);
            } else if presence[0] != 1 {
                return Err(FinalisedStateError::Custom(format!(
                    "invalid Option tag: {}",
                    presence[0]
                )));
            }

            let tx_start = cursor.position();

            cursor.set_position(option_start);
            // Skip this entry to compute length
            Self::skip_opt_transparent_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();
            let slice = &raw[tx_start as usize..end as usize];

            Ok(Some(TransparentCompactTx::from_bytes(slice)?))
        })
    }

    /// Fetch block transparent transaction data by height.
    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        self.read_row_at_height(self.transparent, "transparent", height)
            .await?
            .ok_or_else(|| {
                FinalisedStateError::DataUnavailable("transparent data missing from db".into())
            })
    }

    /// Fetches block transparent tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    ///  NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        self.scan_rows(self.transparent, "transparent", start, end)
            .await
    }

    // *** Internal DB methods ***

    /// Skips one `Option<TransparentCompactTx>` entry from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<TransparentTxList>`
    ///
    /// Advances the cursor past either:
    /// - 1 byte (`0x00`) if `None`, or
    /// - 1 + 1 + vin_size + vout_size if `Some(TransparentCompactTx)`
    ///   (presence + version + variable vin/vout sections)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_transparent_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        let _start_pos = cursor.position();

        // Read 1-byte presence flag
        let mut presence = [0u8; 1];
        cursor.read_exact(&mut presence)?;

        if presence[0] == 0 {
            return Ok(());
        } else if presence[0] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option tag: {}", presence[0]),
            ));
        }

        // Read version (1 byte)
        cursor.read_exact(&mut [0u8; 1])?;

        // Read vin_len (CompactSize)
        let vin_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip vin entries: each is 1-byte version + 36-byte body
        let tx_in_len = TxInCompact::latest_versioned_len()?;
        let vin_skip = vin_len * tx_in_len;
        cursor.set_position(cursor.position() + vin_skip as u64);

        // Read vout_len (CompactSize)
        let vout_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip vout entries: each is 1-byte version + 29-byte body
        let tx_out_len = TxOutCompact::latest_versioned_len()?;
        let vout_skip = vout_len * tx_out_len;
        cursor.set_position(cursor.position() + vout_skip as u64);

        Ok(())
    }
}
