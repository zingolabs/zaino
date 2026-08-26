//! FinalisedState::V1 indexed block indexing functionality.

use super::*;

/// [`IndexedBlockExt`] capability implementation for [`DbV1`].
///
/// Exposes reconstructed [`IndexedBlock`] values from stored per-height entries.
impl IndexedBlockExt for DbV1 {
    async fn get_chain_block(&self, height: Height) -> Result<Option<IndexedBlock>, StoreError> {
        self.get_chain_block(height).await
    }

    async fn get_chain_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<IndexedBlock>, StoreError> {
        self.get_chain_block_range(start, end).await
    }
}

impl DbV1 {
    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_chain_block(&self, height: Height) -> Result<Option<IndexedBlock>, StoreError> {
        let validated_height = match self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await
        {
            Ok(height) => height,
            Err(StoreError::DataUnavailable(_)) => return Ok(None),
            Err(other) => return Err(other),
        };

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            Self::read_chain_block_in_txn(self, &txn, validated_height)
        })
    }

    /// Returns every [`IndexedBlock`] in `start..=end`, ascending.
    ///
    /// One read transaction for the whole range, where calling
    /// [`Self::get_chain_block`] per height opens one each. That is the whole
    /// point: a `GetBlockRange` over a thousand heights used to pay a thousand
    /// `begin_ro_txn` calls and a thousand separate validations, and the reads
    /// were not even coherent with each other — the database could advance
    /// between them.
    ///
    /// A hole in the range is an error, not a skip. The chain head tolerates
    /// gaps because its window genuinely has competing branches; the finalised
    /// state does not, and silently returning a short range would truncate a
    /// wallet's sync without telling it.
    async fn get_chain_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<IndexedBlock>, StoreError> {
        let (validated_start, validated_end) = self.validate_block_range(start, end).await?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut blocks = Vec::with_capacity(
                (validated_end.0.saturating_sub(validated_start.0) as usize).saturating_add(1),
            );

            for height in Height::range_inclusive(validated_start, validated_end) {
                match Self::read_chain_block_in_txn(self, &txn, height)? {
                    Some(block) => blocks.push(block),
                    None => {
                        return Err(StoreError::DataUnavailable(format!(
                            "block at height {height} is missing from a finalised range"
                        )))
                    }
                }
            }

            Ok(blocks)
        })
    }

    /// Reads one block's six rows under an already-open transaction.
    ///
    /// Split out so the single and range reads cannot drift: they assemble the
    /// same block from the same rows, and the only difference is how many
    /// transactions they open to do it.
    fn read_chain_block_in_txn(
        &self,
        txn: &lmdb::RoTransaction<'_>,
        height: Height,
    ) -> Result<Option<IndexedBlock>, StoreError> {
        use lmdb::Transaction as _;

        let height_bytes = height.to_bytes()?;

        {
            // Fetch header data
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let header: BlockHeaderData = *StoredEntryVar::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("header decode error: {e}")))?
                .inner();

            // fetch transaction data
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let txids_list = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("txids decode error: {e}")))?
                .inner()
                .clone();
            let txids = txids_list.txids();

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let transparent_list = StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("transparent decode error: {e}")))?
                .inner()
                .clone();
            let transparent = transparent_list.tx();

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let sapling_list = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("sapling decode error: {e}")))?
                .inner()
                .clone();
            let sapling = sapling_list.tx();

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let orchard_list = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| StoreError::Custom(format!("orchard decode error: {e}")))?
                .inner()
                .clone();
            let orchard = orchard_list.tx();

            // Ironwood (NU6.3): rows only exist from schema v1.3.0 onward, and only for blocks at or
            // above NU6.3 activation. A missing row means the block predates ironwood, so every
            // transaction has an empty ironwood component.
            let ironwood_list = match txn.get(self.ironwood, &height_bytes) {
                Ok(raw) => Some(
                    StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                        .map_err(|e| StoreError::Custom(format!("ironwood decode error: {e}")))?
                        .inner()
                        .clone(),
                ),
                Err(lmdb::Error::NotFound) => None,
                Err(e) => return Err(StoreError::LmdbError(e)),
            };
            let ironwood: &[Option<OrchardCompactTx>] =
                ironwood_list.as_ref().map(|list| list.tx()).unwrap_or(&[]);

            // Build CompactTxData
            let len = txids.len();
            if transparent.len() != len
                || sapling.len() != len
                || orchard.len() != len
                || (!ironwood.is_empty() && ironwood.len() != len)
            {
                return Err(StoreError::Custom(
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
                    let ironwood_tx = ironwood
                        .get(i)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(OrchardCompactTx::empty);

                    CompactTxData::new(
                        i as u64,
                        txid,
                        transparent_tx,
                        sapling_tx,
                        orchard_tx,
                        ironwood_tx,
                    )
                })
                .collect();

            // fetch commitment tree data
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(StoreError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            };

            let commitment_tree_data: CommitmentTreeData =
                *StoredEntryVar::<CommitmentTreeData>::from_bytes(raw)
                    .map_err(|e| StoreError::Custom(format!("commitment_tree decode error: {e}")))?
                    .inner();

            // Construct IndexedBlock
            Ok(Some(IndexedBlock::new(
                header.context,
                *header.data(),
                txs,
                commitment_tree_data,
            )))
        }
    }

    // *** Internal DB methods ***
}
