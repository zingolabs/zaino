//! FinalisedState::V1 shielded block indexing functionality.

use super::*;

use crate::chain_index::ShieldedPool;
use crate::{FixedEncodedLen, ZainoVersionedSerde};

/// How a pool point lookup treats a block height with no row in the pool's table.
enum MissingRow {
    /// Every indexed block has a row in this pool's table; an absent row is an error.
    Error,
    /// The table is sparse: an absent row means the block has no data for this pool.
    NoPoolData,
}

impl MissingRow {
    /// The missing-row policy of `pool`'s table: sapling and orchard rows exist for
    /// every indexed block; pools introduced after schema v1.3.0 use sparse tables.
    fn for_pool(pool: ShieldedPool) -> MissingRow {
        match pool {
            ShieldedPool::Sapling | ShieldedPool::Orchard => MissingRow::Error,
            ShieldedPool::Ironwood => MissingRow::NoPoolData,
        }
    }
}

/// [`BlockShieldedExt`] capability implementation for [`DbV1`].
///
/// Provides access to Sapling / Orchard compact transaction data and per-block commitment tree
/// metadata.
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

    async fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.get_ironwood(tx_location).await
    }

    async fn get_block_ironwood(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.get_block_ironwood(height).await
    }

    async fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.get_block_range_ironwood(start, end).await
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
        self.get_pool_tx(ShieldedPool::Sapling, tx_location)
    }

    /// Fetch block sapling transaction data by height.
    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        self.get_block_pool_tx_list(ShieldedPool::Sapling, height, || {
            SaplingTxList::new(Vec::new())
        })
        .await
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
        self.get_block_range_pool_tx_list(ShieldedPool::Sapling, start, end, || {
            SaplingTxList::new(Vec::new())
        })
        .await
    }

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.get_pool_tx(ShieldedPool::Orchard, tx_location)
    }

    /// Fetch block orchard transaction data by height.
    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.get_block_pool_tx_list(ShieldedPool::Orchard, height, || {
            OrchardTxList::new(Vec::new())
        })
        .await
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
        self.get_block_range_pool_tx_list(ShieldedPool::Orchard, start, end, || {
            OrchardTxList::new(Vec::new())
        })
        .await
    }

    /// Fetch the serialized `OrchardCompactTx` for the given TxLocation from the ironwood table.
    ///
    /// Mirrors [`DbV1::get_orchard`] against the ironwood table (ironwood reuses the Orchard compact
    /// types). A missing ironwood row — any block below NU6.3 activation, or one written before
    /// schema v1.3.0 — yields `Ok(None)` rather than an error.
    async fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.get_pool_tx(ShieldedPool::Ironwood, tx_location)
    }

    /// Fetch block ironwood transaction data by height.
    ///
    /// A missing ironwood row yields an empty [`OrchardTxList`].
    async fn get_block_ironwood(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.get_block_pool_tx_list(ShieldedPool::Ironwood, height, || {
            OrchardTxList::new(Vec::new())
        })
        .await
    }

    /// Fetches block ironwood tx data for the given (inclusive) height range.
    ///
    /// Unlike the orchard range fetch this resolves each height individually so that heights with no
    /// ironwood row (pre-v1.3.0 / pre-NU6.3) yield an empty [`OrchardTxList`], keeping the result
    /// aligned one-entry-per-height with the requested range.
    async fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.get_block_range_pool_tx_list(ShieldedPool::Ironwood, start, end, || {
            OrchardTxList::new(Vec::new())
        })
        .await
    }

    /// Fetch block commitment tree data by height.
    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        self.read_row_at_height(self.commitment_tree_data, "commitment_tree", height)
            .await?
            .ok_or_else(|| {
                FinalisedStateError::DataUnavailable("commitment tree data missing from db".into())
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
        self.scan_rows(self.commitment_tree_data, "commitment_tree", start, end)
            .await
    }

    // *** Internal DB methods ***

    /// The LMDB table holding `pool`'s per-block compact transaction lists.
    fn pool_table(&self, pool: ShieldedPool) -> lmdb::Database {
        match pool {
            ShieldedPool::Sapling => self.sapling,
            ShieldedPool::Orchard => self.orchard,
            ShieldedPool::Ironwood => self.ironwood,
        }
    }

    /// The entry-skip function for `pool`'s tx-list encoding (orchard and ironwood
    /// share the Orchard compact layout).
    fn pool_skip_entry(pool: ShieldedPool) -> fn(&mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        match pool {
            ShieldedPool::Sapling => Self::skip_opt_sapling_entry,
            ShieldedPool::Orchard | ShieldedPool::Ironwood => Self::skip_opt_orchard_entry,
        }
    }

    /// Fetch one pool's whole-block tx list by height. Dense pools error on a missing
    /// row; sparse pools yield `empty()`.
    async fn get_block_pool_tx_list<T: ZainoVersionedSerde>(
        &self,
        pool: ShieldedPool,
        height: Height,
        empty: impl FnOnce() -> T,
    ) -> Result<T, FinalisedStateError> {
        let label = pool.pool_string();
        match self
            .read_row_at_height(self.pool_table(pool), &label, height)
            .await?
        {
            Some(list) => Ok(list),
            None => match MissingRow::for_pool(pool) {
                MissingRow::Error => Err(FinalisedStateError::DataUnavailable(format!(
                    "{label} data missing from db"
                ))),
                MissingRow::NoPoolData => Ok(empty()),
            },
        }
    }

    /// Fetch one pool's tx lists for the inclusive height range, one entry per height.
    ///
    /// Dense pools cursor-scan the range; sparse pools resolve each height individually
    /// so absent rows yield `empty()` and the result stays aligned one-entry-per-height
    /// with the requested range.
    async fn get_block_range_pool_tx_list<T: ZainoVersionedSerde>(
        &self,
        pool: ShieldedPool,
        start: Height,
        end: Height,
        empty: impl Fn() -> T,
    ) -> Result<Vec<T>, FinalisedStateError> {
        match MissingRow::for_pool(pool) {
            MissingRow::Error => {
                self.scan_rows(self.pool_table(pool), &pool.pool_string(), start, end)
                    .await
            }
            MissingRow::NoPoolData => {
                if end.0 < start.0 {
                    return Err(FinalisedStateError::Custom(
                        "invalid block range: end < start".to_string(),
                    ));
                }
                self.validate_block_range(start, end).await?;

                let mut out = Vec::with_capacity((end.0 - start.0 + 1) as usize);
                for height in Height::range_inclusive(start, end) {
                    out.push(self.get_block_pool_tx_list(pool, height, &empty).await?);
                }
                Ok(out)
            }
        }
    }

    /// Point lookup for one transaction's compact data in `pool`'s per-block table,
    /// without decoding the whole block row.
    ///
    /// Walks the `StoredEntryVar` tx-list bytes entry-by-entry with the pool's skip
    /// function, then decodes only the requested entry.
    fn get_pool_tx<T: ZainoVersionedSerde>(
        &self,
        pool: ShieldedPool,
        tx_location: TxLocation,
    ) -> Result<Option<T>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        let table = self.pool_table(pool);
        let label = pool.pool_string();
        let missing_row = MissingRow::for_pool(pool);
        let skip_entry = Self::pool_skip_entry(pool);

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(table, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return match missing_row {
                        MissingRow::NoPoolData => Ok(None),
                        MissingRow::Error => Err(FinalisedStateError::DataUnavailable(format!(
                            "{label} data missing from db"
                        ))),
                    };
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

            // Skip the tx-list version byte
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of entries
            let list_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("{label} tx list len error: {e}"))
            })?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(format!(
                    "tx_index out of range in {label} tx list"
                )));
            }

            // Skip preceding entries
            for _ in 0..idx {
                skip_entry(&mut cursor)
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

            // Rewind to include the presence flag in the returned bytes
            cursor.set_position(start);
            skip_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();

            Ok(Some(T::from_bytes(&raw[start as usize..end as usize])?))
        })
    }

    /// Skips the shared prelude of one `Option<PoolCompactTx>` entry: the presence
    /// byte, and — when the entry is present — the version byte and the `Option<i64>`
    /// value balance. Returns `false` when the entry was `None` (nothing more to skip).
    #[inline]
    fn skip_opt_tx_prelude(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<bool> {
        // Read presence byte
        let mut presence = [0u8; 1];
        cursor.read_exact(&mut presence)?;

        if presence[0] == 0 {
            return Ok(false);
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

        Ok(true)
    }

    /// Advances the cursor past a CompactSize entry count followed by that many
    /// fixed-length entries of type `T`. Taking the width from the type being
    /// skipped makes a wrong-width skip unrepresentable.
    #[inline]
    fn skip_counted_fixed_entries<T: FixedEncodedLen + ZainoVersionedSerde>(
        cursor: &mut std::io::Cursor<&[u8]>,
    ) -> io::Result<()> {
        let count = CompactSize::read(&mut *cursor)? as usize;
        let entry_len = T::latest_versioned_len()?;
        cursor.set_position(cursor.position() + (count * entry_len) as u64);
        Ok(())
    }

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
        if !Self::skip_opt_tx_prelude(cursor)? {
            return Ok(());
        }
        Self::skip_counted_fixed_entries::<CompactSaplingSpend>(cursor)?;
        Self::skip_counted_fixed_entries::<crate::CompactSaplingOutput>(cursor)
    }

    /// Skips one `Option<OrchardCompactTx>` from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<OrchardTxList>` (the orchard and ironwood tables share this
    ///   layout)
    ///
    /// Advances past:
    /// - 1 byte `0x00` if None, or
    /// - 1 + 1 + value + actions if Some (presence + version + body)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_orchard_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        if !Self::skip_opt_tx_prelude(cursor)? {
            return Ok(());
        }
        Self::skip_counted_fixed_entries::<CompactOrchardAction>(cursor)
    }
}

#[cfg(test)]
mod skip_opt_sapling_entry {
    use super::*;
    use crate::CompactSaplingOutput;

    /// Regression test for the sapling point-lookup skip width: the skip must advance
    /// exactly one encoded `Option<SaplingCompactTx>` entry. A refactor had it skipping
    /// sapling *outputs* at the spend width (33 bytes instead of 117), landing the cursor
    /// mid-record for any entry with outputs and corrupting every `get_sapling` read of a
    /// transaction at or after such an entry.
    #[test]
    fn advances_exactly_one_encoded_entry() {
        let tx = SaplingCompactTx::new(
            Some(7),
            vec![CompactSaplingSpend::new([1u8; 32])],
            vec![
                CompactSaplingOutput::new([2u8; 32], [3u8; 32], [4u8; 52]),
                CompactSaplingOutput::new([5u8; 32], [6u8; 32], [7u8; 52]),
            ],
        );
        let list = SaplingTxList::new(vec![Some(tx)]);
        let bytes = list
            .to_bytes()
            .expect("encoding an in-memory list cannot fail");

        let mut cursor = std::io::Cursor::new(bytes.as_slice());
        // Mirror `get_sapling`: skip the SaplingTxList version byte, then the entry count.
        cursor.set_position(1);
        CompactSize::read(&mut cursor).expect("entry count is present");

        DbV1::skip_opt_sapling_entry(&mut cursor).expect("entry is well-formed");

        assert_eq!(
            cursor.position() as usize,
            bytes.len(),
            "skip_opt_sapling_entry must land exactly on the end of the single encoded entry"
        );
    }
}
