//! FinalisedState::V1 core read functionality.

use super::*;

use crate::codec::DbCodec;

/// [`DbRead`] capability implementation for [`DbV1`].
///
/// This trait is the read-only surface used by higher layers. Methods typically delegate to
/// inherent async helpers that confirm the requested heights are stored.
impl DbRead for DbV1 {
    async fn db_height(&self) -> Result<Option<Height>, StoreError> {
        self.tip_height().await
    }

    async fn get_block_height(&self, hash: BlockHash) -> Result<Option<Height>, StoreError> {
        match self.get_block_height_by_hash(hash).await {
            Ok(height) => Ok(Some(height)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn get_block_hash(&self, height: Height) -> Result<Option<BlockHash>, StoreError> {
        match self.get_block_header_data(height).await {
            Ok(header) => Ok(Some(header.context.index.hash)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the greatest `Height` stored in `headers`
    /// (`None` if the DB is still empty).
    pub(crate) async fn tip_height(&self) -> Result<Option<Height>, StoreError> {
        #[cfg(test)]
        self.tip_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            .resolve_stored_height(HashOrHeight::Hash(hash.into()))
            .await?;
        Ok(height)
    }

    /// Fetch database metadata.
    #[cfg(test)]
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        self.read_row(self.metadata, "metadata", METADATA_KEY)?
            .ok_or_else(|| StoreError::DataUnavailable("metadata missing from db".into()))
    }

    /// Resolves `hash_or_height` to a stored height, or `DataUnavailable` when the store does not hold it.
    pub(super) async fn resolve_stored_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, StoreError> {
        match hash_or_height {
            HashOrHeight::Height(z_height) => {
                let height = Height::try_from(z_height.0)
                    .map_err(|_| StoreError::DataUnavailable("height out of range".into()))?;
                if self.tip_height().await?.is_some_and(|tip| height <= tip) {
                    Ok(height)
                } else {
                    Err(not_stored())
                }
            }
            HashOrHeight::Hash(z_hash) => {
                let hkey = BlockHash::from(z_hash.0).to_bytes()?;
                tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.heights, &hkey).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            not_stored()
                        } else {
                            StoreError::LmdbError(e)
                        }
                    })?;
                    Ok(Height::from_bytes(bytes)?)
                })
            }
        }
    }

    /// Confirms that the inclusive range between `start` and `end`, in either order, lies at or below the stored tip.
    pub(super) async fn require_stored_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<(), StoreError> {
        let highest = std::cmp::max(start, end);
        if self.tip_height().await?.is_some_and(|tip| highest <= tip) {
            Ok(())
        } else {
            Err(not_stored())
        }
    }
}

/// The one answer for a height or hash the store does not hold, whichever read asked.
fn not_stored() -> StoreError {
    StoreError::DataUnavailable("height not found in best chain".into())
}

impl DbV1 {
    /// Fetches and decodes one `T` row keyed by `key`, returning `Ok(None)` when the table has no row there.
    pub(super) fn read_row<T: DbCodec>(
        &self,
        table: lmdb::Database,
        label: &str,
        key: &[u8],
    ) -> Result<Option<T>, StoreError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(table, &key) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            T::from_bytes(raw)
                .map(Some)
                .map_err(|e| StoreError::Custom(format!("{label} decode error: {e}")))
        })
    }

    /// [`DbV1::read_row`] keyed by `height`, where an absent row means the caller's table has none there and the caller has established whether the height is stored.
    pub(super) async fn read_row_at_height<T: DbCodec>(
        &self,
        table: lmdb::Database,
        label: &str,
        height: Height,
    ) -> Result<Option<T>, StoreError> {
        let height_bytes = height.to_bytes()?;
        self.read_row(table, label, &height_bytes)
    }
}
