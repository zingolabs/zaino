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
    ) -> Result<Option<TransparentCompactTx>, StoreError> {
        self.get_transparent(tx_location).await
    }

    async fn get_previous_output(&self, outpoint: Outpoint) -> Result<TxOutCompact, StoreError> {
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
    ) -> Result<Option<TransparentCompactTx>, StoreError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| StoreError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Read CompactSize: number of records
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| StoreError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(StoreError::Custom(
                    "tx_index out of range in transparent tx data".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_transparent_entry(&mut cursor)
                    .map_err(|e| StoreError::Custom(format!("skip entry error: {e}")))?;
            }

            let option_start = cursor.position();

            // Peek at the 1-byte presence flag
            let mut presence = [0u8; 1];
            cursor
                .read_exact(&mut presence)
                .map_err(|e| StoreError::Custom(format!("failed to read Option tag: {e}")))?;

            if presence[0] == 0 {
                return Ok(None);
            } else if presence[0] != 1 {
                return Err(StoreError::Custom(format!(
                    "invalid Option tag: {}",
                    presence[0]
                )));
            }

            let tx_start = cursor.position();

            cursor.set_position(option_start);
            // Skip this entry to compute length
            Self::skip_opt_transparent_entry(&mut cursor)
                .map_err(|e| StoreError::Custom(format!("skip entry error (second pass): {e}")))?;

            let end = cursor.position();
            let slice = &raw[tx_start as usize..end as usize];

            Ok(Some(TransparentCompactTx::from_bytes(slice)?))
        })
    }

    // *** Internal DB methods ***

    /// Skips one `Option<TransparentCompactTx>` entry from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - stored `TransparentTxList`
    ///
    /// Advances the cursor past either:
    /// - 1 byte (`0x00`) if `None`, or
    /// - 1 + vin_size + vout_size if `Some(TransparentCompactTx)`
    ///   (presence + variable vin/vout sections)
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

        // Read vin_len (CompactSize), then skip the fixed-length vin entries
        let vin_len = CompactSize::read(&mut *cursor)? as usize;
        let vin_skip = vin_len * TxInCompact::ENCODED_LEN;
        cursor.set_position(cursor.position() + vin_skip as u64);

        // Read vout_len (CompactSize), then skip the fixed-length vout entries
        let vout_len = CompactSize::read(&mut *cursor)? as usize;
        let vout_skip = vout_len * TxOutCompact::ENCODED_LEN;
        cursor.set_position(cursor.position() + vout_skip as u64);

        Ok(())
    }
}
