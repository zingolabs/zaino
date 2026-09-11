//! FinalisedState::V1 core read functionality.

use super::*;

use zaino_encoding::ZainoVersionedSerde;

/// [`DbRead`] capability implementation for [`DbV1`].
///
/// This trait is the read-only surface used by higher layers. Methods typically delegate to
/// inherent async helpers that enforce validated reads where required.
impl DbRead for DbV1 {
    async fn db_height(&self) -> Result<Option<Height>, StoreError> {
        self.tip_height().await
    }

    async fn get_block_height(&self, hash: BlockHash) -> Result<Option<Height>, StoreError> {
        match self.get_block_height_by_hash(hash).await {
            Ok(height) => Ok(Some(height)),
            Err(StoreError::DataUnavailable(_) | StoreError::FeatureUnavailable(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn get_block_hash(&self, height: Height) -> Result<Option<BlockHash>, StoreError> {
        match self.get_block_header_data(height).await {
            Ok(header) => Ok(Some(header.context.index.hash)),
            Err(StoreError::DataUnavailable(_) | StoreError::FeatureUnavailable(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        self.get_metadata().await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the greatest `Height` stored in `headers`
    /// (`None` if the DB is still empty).
    pub(crate) async fn tip_height(&self) -> Result<Option<Height>, StoreError> {
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.headers)?;

            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((key_bytes, _val_bytes)) => {
                    // `key_bytes` is exactly what `Height::to_bytes()` produced
                    let h = Height::from_bytes(
                        key_bytes.expect("height is always some in the finalised state"),
                    )
                    .map_err(|e| StoreError::Custom(format!("height decode: {e}")))?;
                    Ok(Some(h))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(e) => Err(StoreError::LmdbError(e)),
            }
        })
    }

    /// Fetch the block height in the main chain for a given block hash.
    async fn get_block_height_by_hash(&self, hash: BlockHash) -> Result<Height, StoreError> {
        let height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(hash.into()))
            .await?;
        Ok(height)
    }

    /// Fetch the height range for the given block hashes.
    async fn get_block_range_by_hash(
        &self,
        start_hash: BlockHash,
        end_hash: BlockHash,
    ) -> Result<(Height, Height), StoreError> {
        let start_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(start_hash.into()))
            .await?;
        let end_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(end_hash.into()))
            .await?;

        let (validated_start, validated_end) =
            self.validate_block_range(start_height, end_height).await?;

        Ok((validated_start, validated_end))
    }

    /// Fetch database metadata.
    async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.metadata, b"metadata") {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            let entry = StoredEntryFixed::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("metadata decode error: {e}")))?;

            Ok(entry.into_inner())
        })
    }

    // *** Internal DB methods ***
}

impl DbV1 {
    /// Fetches and decodes one `StoredEntryVar<T>` row keyed by an already-validated
    /// height. Returns `Ok(None)` when the table has no row for the height; `label`
    /// names the table in decode errors.
    fn read_row<T: ZainoVersionedSerde>(
        &self,
        table: lmdb::Database,
        label: &str,
        height_bytes: &[u8],
    ) -> Result<Option<T>, StoreError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(table, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let entry: StoredEntryVar<T> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("{label} decode error: {e}")))?;
            Ok(Some(entry.into_inner()))
        })
    }

    /// [`DbV1::read_row`] at a height that is first validated against the index.
    pub(super) async fn read_row_at_height<T: ZainoVersionedSerde>(
        &self,
        table: lmdb::Database,
        label: &str,
        height: Height,
    ) -> Result<Option<T>, StoreError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;
        self.read_row(table, label, &height_bytes)
    }

    /// Cursor-scans and decodes every `StoredEntryVar<T>` row in the validated
    /// inclusive `start..=end` height range.
    pub(super) async fn scan_rows<T: ZainoVersionedSerde>(
        &self,
        table: lmdb::Database,
        label: &str,
        start: Height,
        end: Height,
    ) -> Result<Vec<T>, StoreError> {
        if end.0 < start.0 {
            return Err(StoreError::Custom(
                "invalid block range: end < start".to_string(),
            ));
        }

        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(table) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(format!(
                        "{label} data missing from db"
                    )));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, StoreError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<T>::from_bytes(&bytes)
                    .map(|e| e.into_inner())
                    .map_err(|e| StoreError::Custom(format!("{label} decode error: {e}")))
            })
            .collect()
    }
}
