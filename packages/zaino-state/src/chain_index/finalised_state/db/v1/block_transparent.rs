//! ZainoDB::V1 transparent block indexing functionality.

use super::*;

/// [`BlockTransparentExt`] capability implementation for [`DbV1`].
///
/// Provides access to transparent compact transaction data at both per-transaction and per-block
/// granularity.
#[async_trait]
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

            let start = cursor.position();

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

            cursor.set_position(start);
            // Skip this entry to compute length
            Self::skip_opt_transparent_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();
            let slice = &raw[start as usize..end as usize];

            Ok(Some(TransparentCompactTx::from_bytes(slice)?))
        })
    }

    /// Fetch block transparent transaction data by height.
    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<TransparentTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("transparent decode error: {e}"))
                })?;

            Ok(entry.inner().clone())
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
            let mut cursor = match txn.open_ro_cursor(self.transparent) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
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
                StoredEntryVar::<TransparentTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!("transparent decode error: {e}"))
                    })
            })
            .collect()
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
        let vin_skip = vin_len * TxInCompact::VERSIONED_LEN;
        cursor.set_position(cursor.position() + vin_skip as u64);

        // Read vout_len (CompactSize)
        let vout_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip vout entries: each is 1-byte version + 29-byte body
        let vout_skip = vout_len * TxOutCompact::VERSIONED_LEN;
        cursor.set_position(cursor.position() + vout_skip as u64);

        Ok(())
    }
}
