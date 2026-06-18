//! FinalisedState::V1 shielded block indexing functionality.

use super::*;

/// [`BlockShieldedExt`] capability implementation for [`DbV1`].
///
/// Provides access to Sapling / Orchard compact transaction data and per-block commitment tree
/// metadata.
#[async_trait]
impl BlockShieldedExt for DbV1 {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        self.get_sapling(tx_location).await
    }

    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        self.get_block_sapling(height).await
    }

    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        self.get_block_range_sapling(start, end).await
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.get_orchard(tx_location).await
    }

    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.get_block_orchard(height).await
    }

    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.get_block_range_orchard(start, end).await
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        self.get_block_commitment_tree_data(height).await
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        self.get_block_range_commitment_tree_data(start, end).await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Skip SaplingTxList version byte
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of entries
            let list_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("sapling tx list len error: {e}"))
            })?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in sapling tx list".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_sapling_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let start = cursor.position();

            // Peek presence flag
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

            // Rewind to include tag in returned bytes
            cursor.set_position(start);
            Self::skip_opt_sapling_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();

            Ok(Some(SaplingCompactTx::from_bytes(
                &raw[start as usize..end as usize],
            )?))
        })
    }

    /// Fetch block sapling transaction data by height.
    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<SaplingTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block sapling tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    /// NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
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
            let mut cursor = match txn.open_ro_cursor(self.sapling) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
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
                StoredEntryVar::<SaplingTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))
            })
            .collect()
    }

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Skip OrchardTxList version byte
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of entries
            let list_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("orchard tx list len error: {e}"))
            })?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in orchard tx list".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_orchard_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let start = cursor.position();

            // Peek presence flag
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

            // Rewind to include presence flag in output
            cursor.set_position(start);
            Self::skip_opt_orchard_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();

            Ok(Some(OrchardCompactTx::from_bytes(
                &raw[start as usize..end as usize],
            )?))
        })
    }

    /// Fetch block orchard transaction data by height.
    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<OrchardTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block orchard tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    /// NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
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
            let mut cursor = match txn.open_ro_cursor(self.orchard) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
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
                StoredEntryVar::<OrchardTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))
            })
            .collect()
    }

    /// Fetch block commitment tree data by height.
    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "commitment tree data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry = StoredEntryFixed::from_bytes(raw).map_err(|e| {
                FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
            })?;

            Ok(entry.item)
        })
    }

    /// Fetches block commitment tree data for the given height range.
    ///
    /// Uses cursor based fetch.
    ///
    /// NOTE: Currently this method only fetches ranges where start_height <= end_height,
    ///       This could be updated by following the cursor step example in
    ///       get_compact_block_streamer.
    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
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
            let mut cursor = match txn.open_ro_cursor(self.commitment_tree_data) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "commitment tree data missing from db".into(),
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
                StoredEntryFixed::<CommitmentTreeData>::from_bytes(&bytes)
                    .map(|e| e.item)
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                    })
            })
            .collect()
    }

    // *** Internal DB methods ***

    /// Skips one `Option<SaplingCompactTx>` from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<SaplingTxList>`
    ///
    /// Advances past:
    /// - 1 byte `0x00` if None, or
    /// - 1 + 1 + value + spends + outputs if Some (presence + version + body)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_sapling_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        // Read presence byte
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

        // Read version
        cursor.read_exact(&mut [0u8; 1])?;

        // Read value: Option<i64>
        let mut value_tag = [0u8; 1];
        cursor.read_exact(&mut value_tag)?;
        if value_tag[0] == 1 {
            // Some(i64): read 8 bytes
            cursor.set_position(cursor.position() + 8);
        } else if value_tag[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option<i64> tag: {}", value_tag[0]),
            ));
        }

        // Read number of spends (CompactSize)
        let spend_len = CompactSize::read(&mut *cursor)? as usize;
        let spend_skip = spend_len * CompactSaplingSpend::VERSIONED_LEN;
        cursor.set_position(cursor.position() + spend_skip as u64);

        // Read number of outputs (CompactSize)
        let output_len = CompactSize::read(&mut *cursor)? as usize;
        let output_skip = output_len * CompactSaplingOutput::VERSIONED_LEN;
        cursor.set_position(cursor.position() + output_skip as u64);

        Ok(())
    }

    /// Skips one `Option<OrchardCompactTx>` from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<OrchardTxList>`
    ///
    /// Advances past:
    /// - 1 byte `0x00` if None, or
    /// - 1 + 1 + value + actions if Some (presence + version + body)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_orchard_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        // Read presence byte
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

        // Read version
        cursor.read_exact(&mut [0u8; 1])?;

        // Read value: Option<i64>
        let mut value_tag = [0u8; 1];
        cursor.read_exact(&mut value_tag)?;
        if value_tag[0] == 1 {
            // Some(i64): read 8 bytes
            cursor.set_position(cursor.position() + 8);
        } else if value_tag[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option<i64> tag: {}", value_tag[0]),
            ));
        }

        // Read number of actions (CompactSize)
        let action_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip actions: each is 1-byte version + 148-byte body
        let action_skip = action_len * CompactOrchardAction::VERSIONED_LEN;
        cursor.set_position(cursor.position() + action_skip as u64);

        Ok(())
    }
}
