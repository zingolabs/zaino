//! ZainoDB::V1 indexed block indexing functionality.

use super::*;

/// [`IndexedBlockExt`] capability implementation for [`DbV1`].
///
/// Exposes reconstructed [`IndexedBlock`] values from stored per-height entries.
#[async_trait]
impl IndexedBlockExt for DbV1 {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        self.get_chain_block(height).await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        let validated_height = match self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await
        {
            Ok(height) => height,
            Err(FinalisedStateError::DataUnavailable(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            // Fetch header data
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let header: BlockHeaderData = *StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?
                .inner();

            // fetch transaction data
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let txids_list = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?
                .inner()
                .clone();
            let txids = txids_list.txids();

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let transparent_list = StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("transparent decode error: {e}")))?
                .inner()
                .clone();
            let transparent = transparent_list.tx();

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let sapling_list = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))?
                .inner()
                .clone();
            let sapling = sapling_list.tx();

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let orchard_list = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))?
                .inner()
                .clone();
            let orchard = orchard_list.tx();

            // Build CompactTxData
            let len = txids.len();
            if transparent.len() != len || sapling.len() != len || orchard.len() != len {
                return Err(FinalisedStateError::Custom(
                    "mismatched tx list lengths in block data".to_string(),
                ));
            }

            let txs: Vec<CompactTxData> = (0..len)
                .map(|i| {
                    let txid = txids[i];
                    let transparent_tx = transparent[i]
                        .clone()
                        .unwrap_or_else(|| TransparentCompactTx::new(vec![], vec![]));
                    let sapling_tx = sapling[i]
                        .clone()
                        .unwrap_or_else(|| SaplingCompactTx::new(None, vec![], vec![]));
                    let orchard_tx = orchard[i]
                        .clone()
                        .unwrap_or_else(|| OrchardCompactTx::new(None, vec![]));

                    CompactTxData::new(i as u64, txid, transparent_tx, sapling_tx, orchard_tx)
                })
                .collect();

            // fetch commitment tree data
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let commitment_tree_data: CommitmentTreeData = *StoredEntryFixed::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                })?
                .inner();

            // Construct IndexedBlock
            Ok(Some(IndexedBlock::new(
                *header.index(),
                *header.data(),
                txs,
                commitment_tree_data,
            )))
        })
    }

    // *** Internal DB methods ***
}
