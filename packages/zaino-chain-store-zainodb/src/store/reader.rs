//! Read-only view onto a running `FinalisedState` (DbReader)
//!
//! This file defines [`DbReader`], the **read-only** interface that should be used for *all* chain
//! data fetches from the finalised database.
//!
//! `DbReader` narrows the surface to reads and discourages accidental use of write APIs from query
//! paths. Every method reads the one database directly; an optional index's methods exist only
//! when its cargo feature is enabled.
//!
//! # Development: adding a new read method
//!
//! 1. Decide whether the new query belongs under an existing extension trait or needs a new one.
//! 2. Implement the extension trait for `DbV1`, gated by a cargo feature if the index is optional.
//! 3. Add the new method on `DbReader`, which calls into the database.
//!
//! # Usage pattern
//!
//! `DbReader` is created from an `Arc<FinalisedState>` using [`FinalisedState::to_reader`](super::FinalisedState::to_reader).
//! Prefer passing `DbReader` through query layers rather than passing `FinalisedState` directly.
//!
//! # The `pub` methods are the legacy surface, not the interface
//!
//! Twelve of these are `pub`. They are what `zaino-state`'s ChainIndex still
//! calls while it is moved onto the ports, and they are `pub` for that reason
//! alone — they speak this backend's on-disk vocabulary, which no consumer
//! should be written against. [`zaino_chain_store`]'s traits are the interface;
//! this crate implements them in [`crate::ports`]. Every method here has a port
//! that supersedes it, and the set shrinks to nothing rather than growing.
//!
//! Everything else stays private: they are internal assembly steps, and the
//! distinction between the two is the point of the split.

use zaino_proto::proto::utils::PoolTypeFilter;

use crate::error::StoreError;
use crate::stream::CompactBlockStream;
use crate::types::{
    db::metadata::FinalisedTxOutSetInfoAccumulator, AbsoluteChainWork, BlockHash, BlockHeaderData,
    CommitmentTreeData, Height, IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint,
    SaplingCompactTx, SaplingTxList, TransactionHash, TransparentCompactTx, TransparentTxList,
    TxLocation, TxOutCompact, TxidList,
};
use zaino_chain_store::ChainStoreSource;
use zaino_status::StatusType;

#[cfg(feature = "transparent_address_history_experimental")]
use crate::store::capability::{AddrUtxo, TransparentHistExt};
#[cfg(feature = "transparent_address_history_experimental")]
use crate::types::{AddrEventBytes, AddrScript};

use super::{
    capability::{
        BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbMetadata,
        IndexedBlockExt, SpentOutputExt, TxOutSetExt,
    },
    finalised_source::v1::DbV1,
    FinalisedState,
};

use std::sync::Arc;

/// Cheaply cloneable, read-only entry point for chain queries.
pub struct DbReader<T: ChainStoreSource> {
    /// Shared handle to the running `FinalisedState` instance.
    pub(crate) inner: Arc<FinalisedState<T>>,
}

/// Cloned and formatted by hand rather than derived.
///
/// Both derives would demand the bound of the same name on `T`, and a validator
/// is neither `Clone` nor necessarily `Debug`. The one field is an [`Arc`], so
/// neither bound is needed to do the work.
impl<T: ChainStoreSource> Clone for DbReader<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: ChainStoreSource> std::fmt::Debug for DbReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbReader").finish_non_exhaustive()
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// Returns the database every read goes to.
    #[inline(always)]
    fn db(&self) -> &DbV1 {
        self.inner.backend()
    }

    // ***** DB Core Read *****

    /// Returns the current runtime status of the database.
    pub fn status(&self) -> StatusType {
        self.inner.status()
    }

    /// Returns the greatest block `Height` stored in the database, or `None` if the DB is empty.
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, StoreError> {
        self.inner.db_height().await
    }

    /// Fetches the persisted database metadata singleton (`DbMetadata`).
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        self.inner.get_metadata().await
    }

    /// Waits until the database reports [`StatusType::Ready`].
    pub(crate) async fn wait_until_ready(&self) {
        self.inner.wait_until_ready().await
    }

    /// Fetches the main-chain height for a given block hash, if present in finalised state.
    pub async fn get_block_height(&self, hash: BlockHash) -> Result<Option<Height>, StoreError> {
        self.inner.get_block_height(hash).await
    }

    /// Fetches the main-chain block hash for a given block height, if present in finalised state.
    pub async fn get_block_hash(&self, height: Height) -> Result<Option<BlockHash>, StoreError> {
        self.inner.get_block_hash(height).await
    }

    // ***** Block Core Ext *****

    /// Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    pub async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, StoreError> {
        self.db().get_tx_location(txid).await
    }

    /// Fetch block header data by height.
    pub(crate) async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, StoreError> {
        self.db().get_block_header(height).await
    }

    /// Fetches block headers for the given height range.
    pub(crate) async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, StoreError> {
        self.db().get_block_range_headers(start, end).await
    }

    /// Fetch the txid bytes for a given TxLocation.
    pub async fn get_txid(&self, tx_location: TxLocation) -> Result<TransactionHash, StoreError> {
        self.db().get_txid(tx_location).await
    }

    /// Fetch block txids by height.
    pub(crate) async fn get_block_txids(&self, height: Height) -> Result<TxidList, StoreError> {
        self.db().get_block_txids(height).await
    }

    /// Fetches block txids for the given height range.
    pub(crate) async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, StoreError> {
        self.db().get_block_range_txids(start, end).await
    }

    // ***** Block Transparent Ext *****

    /// Fetch the serialized TransparentCompactTx for the given TxLocation, if present.
    pub async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, StoreError> {
        self.db().get_transparent(tx_location).await
    }

    /// Fetch block transparent transaction data by height.
    pub(crate) async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, StoreError> {
        self.db().get_block_transparent(height).await
    }

    /// Fetches block transparent tx data for the given height range.
    pub(crate) async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, StoreError> {
        self.db().get_block_range_transparent(start, end).await
    }

    // ***** Block shielded Ext *****

    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    pub(crate) async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, StoreError> {
        self.db().get_sapling(tx_location).await
    }

    /// Fetch block sapling transaction data by height.
    pub(crate) async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, StoreError> {
        self.db().get_block_sapling(height).await
    }

    /// Fetches block sapling tx data for the given height range.
    pub(crate) async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, StoreError> {
        self.db().get_block_range_sapling(start, end).await
    }

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    pub(crate) async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, StoreError> {
        self.db().get_orchard(tx_location).await
    }

    /// Fetch block orchard transaction data by height.
    pub(crate) async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, StoreError> {
        self.db().get_block_orchard(height).await
    }

    /// Fetches block orchard tx data for the given height range.
    pub(crate) async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, StoreError> {
        self.db().get_block_range_orchard(start, end).await
    }

    /// Fetch the serialized ironwood (NU6.3) compact tx for the given TxLocation, if present.
    pub(crate) async fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, StoreError> {
        self.db().get_ironwood(tx_location).await
    }

    /// Fetch block ironwood transaction data by height.
    pub(crate) async fn get_block_ironwood(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, StoreError> {
        self.db().get_block_ironwood(height).await
    }

    /// Fetches block ironwood tx data for the given height range.
    pub(crate) async fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, StoreError> {
        self.db().get_block_range_ironwood(start, end).await
    }

    /// Fetch block commitment tree data by height.
    pub(crate) async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, StoreError> {
        self.db().get_block_commitment_tree_data(height).await
    }

    /// Fetches block commitment tree data for the given height range.
    pub(crate) async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, StoreError> {
        self.db()
            .get_block_range_commitment_tree_data(start, end)
            .await
    }

    // ***** Transparent Hist Ext *****

    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
        self.db().addr_records(addr_script).await
    }

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, StoreError> {
        self.db()
            .addr_and_index_records(addr_script, tx_location)
            .await
    }

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, StoreError> {
        self.db()
            .addr_tx_locations_by_range(addr_script, start_height, end_height)
            .await
    }

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<AddrUtxo>>, StoreError> {
        self.db()
            .addr_utxos_by_range(addr_script, start_height, end_height)
            .await
    }

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, StoreError> {
        self.db()
            .addr_balance_by_range(addr_script, start_height, end_height)
            .await
    }

    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    pub(crate) async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, StoreError> {
        self.db().get_outpoint_spender(outpoint).await
    }

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    pub async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, StoreError> {
        self.db().get_outpoint_spenders(outpoints).await
    }

    /// Returns the finalised-state txout-set accumulator.
    pub async fn get_tx_out_set_info_accumulator(
        &self,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, StoreError> {
        self.db().get_tx_out_set_info_accumulator().await
    }

    /// Returns the previous transparent output referenced by `outpoint`, which `gettxoutsetinfo` uses to resolve non-finalised spends.
    pub async fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, StoreError> {
        self.db().get_previous_output(outpoint).await
    }

    // ***** IndexedBlock Ext *****

    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    pub async fn get_chain_block_by_height(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, StoreError> {
        self.db().get_chain_block(height).await
    }

    /// Returns every stored `IndexedBlock` in `start..=end`, ascending, under one read transaction.
    pub(crate) async fn get_stored_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<IndexedBlock<AbsoluteChainWork>>, StoreError> {
        self.db().get_stored_block_range(start, end).await
    }

    pub(crate) async fn get_chain_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<IndexedBlock>, StoreError> {
        let Some(height) = self.inner.get_block_height(hash).await? else {
            return Ok(None);
        };
        self.get_chain_block_by_height(height).await
    }

    // ***** CompactBlock Ext *****

    /// Returns the compact block at `height`, as a wire message.
    ///
    /// # Temporary
    ///
    /// The backend produces a domain block; this converts it for consumers that
    /// have not moved onto the ports. It goes when they do, along with this
    /// crate's dependency on `zaino-proto`.
    pub async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, StoreError> {
        let block = self
            .db()
            .get_compact_block(
                height,
                crate::store::finalised_source::v1::compact_block::pool_filter_from_wire(
                    &pool_types,
                ),
            )
            .await?;
        Ok(crate::store::finalised_source::v1::compact_block::compact_block_to_wire(&block))
    }

    /// Returns every compact block in `start..=end`, ascending, under one read
    /// transaction.
    pub(crate) async fn get_compact_block_range(
        &self,
        start: Height,
        end: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<zaino_primitives::types::CompactBlock>, StoreError> {
        self.db()
            .get_compact_block_range(start, end, pool_types)
            .await
    }

    /// Streams compact blocks over an inclusive height range, as wire messages.
    ///
    /// # Temporary
    ///
    /// Carries `tonic::Status` as its error, which is a serving concern with no
    /// business in a storage crate. Superseded by
    /// [`zaino_chain_store::CompactBlockRead::compact_stream`], which carries a
    /// `ChainStoreError` and yields domain blocks.
    pub async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, StoreError> {
        self.db()
            .get_compact_block_stream(start_height, end_height, pool_types)
            .await
    }
}
