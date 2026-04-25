//! ZainoDB::V1 core read functionality.

use super::*;

/// [`DbRead`] capability implementation for [`DbV1`].
///
/// This trait is the read-only surface used by higher layers. Methods typically delegate to
/// inherent async helpers that enforce validated reads where required.
#[async_trait]
impl DbRead for DbV1 {
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.tip_height().await
    }

    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        match self.get_block_height_by_hash(hash).await {
            Ok(height) => Ok(Some(height)),
            Err(
                FinalisedStateError::DataUnavailable(_)
                | FinalisedStateError::FeatureUnavailable(_),
            ) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        match self.get_block_header_data(height).await {
            Ok(header) => Ok(Some(header.context.index.hash)),
            Err(
                FinalisedStateError::DataUnavailable(_)
                | FinalisedStateError::FeatureUnavailable(_),
            ) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.get_metadata().await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the greatest `Height` stored in `headers`
    /// (`None` if the DB is still empty).
    pub(crate) async fn tip_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.headers)?;

            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((key_bytes, _val_bytes)) => {
                    // `key_bytes` is exactly what `Height::to_bytes()` produced
                    let h = Height::from_bytes(
                        key_bytes.expect("height is always some in the finalised state"),
                    )
                    .map_err(|e| FinalisedStateError::Custom(format!("height decode: {e}")))?;
                    Ok(Some(h))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        })
    }

    /// Fetch the block height in the main chain for a given block hash.
    async fn get_block_height_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Height, FinalisedStateError> {
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
    ) -> Result<(Height, Height), FinalisedStateError> {
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
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.metadata, b"metadata") {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry = StoredEntryFixed::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("metadata decode error: {e}")))?;

            Ok(entry.item)
        })
    }

    // *** Internal DB methods ***
}
