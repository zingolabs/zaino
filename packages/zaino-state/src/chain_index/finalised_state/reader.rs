//! Read-only view onto a running `ZainoDB` (DbReader)
//!
//! This file defines [`DbReader`], the **read-only** interface that should be used for *all* chain
//! data fetches from the finalised database.
//!
//! `DbReader` exists for two reasons:
//!
//! 1. **API hygiene:** it narrows the surface to reads and discourages accidental use of write APIs
//!    from query paths.
//! 2. **Migration safety:** it routes each call through [`Router`](super::router::Router) using a
//!    [`CapabilityRequest`](crate::chain_index::finalised_state::capability::CapabilityRequest),
//!    ensuring the underlying backend supports the requested feature (especially important during
//!    major migrations where different DB versions may coexist).
//!
//! # How routing works
//!
//! Each method in `DbReader` requests a specific capability (e.g. `BlockCoreExt`, `TransparentHistExt`).
//! Internally, `DbReader::db(cap)` calls `ZainoDB::backend_for_cap(cap)`, which consults the router.
//!
//! - If the capability is currently served by the shadow DB (shadow mask contains the bit), the
//!   query runs against shadow.
//! - Otherwise, it runs against primary if primary supports it.
//! - If neither backend supports it, the call returns `FinalisedStateError::FeatureUnavailable(...)`.
//!
//! # Version constraints and error handling
//!
//! Some queries are only available in newer DB versions (notably most v1 extension traits).
//! Callers should either:
//! - require a minimum DB version (via configuration and/or metadata checks), or
//! - handle `FeatureUnavailable` errors gracefully when operating against legacy databases.
//!
//! # Development: adding a new read method
//!
//! 1. Decide whether the new query belongs under an existing extension trait or needs a new one.
//! 2. If a new capability is required:
//!    - add a new `Capability` bit and `CapabilityRequest` variant in `capability.rs`,
//!    - implement the corresponding extension trait for supported DB versions,
//!    - delegate through `DbBackend` and route via the router.
//! 3. Add the new method on `DbReader` that requests the corresponding `CapabilityRequest` and calls
//!    into the backend.
//!
//! # Usage pattern
//!
//! `DbReader` is created from an `Arc<ZainoDB>` using [`ZainoDB::to_reader`](super::ZainoDB::to_reader).
//! Prefer passing `DbReader` through query layers rather than passing `ZainoDB` directly.

use zaino_proto::proto::utils::PoolTypeFilter;

use crate::{
    chain_index::{finalised_state::capability::CapabilityRequest, types::TransactionHash},
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream, Height, IndexedBlock,
    OrchardCompactTx, OrchardTxList, SaplingCompactTx, SaplingTxList, StatusType,
    TransparentCompactTx, TransparentTxList, TxLocation, TxidList,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{
    chain_index::{finalised_state::capability::TransparentHistExt, types::AddrEventBytes},
    AddrScript, Outpoint,
};

use super::{
    capability::{
        BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbMetadata,
        IndexedBlockExt,
    },
    db::DbBackend,
    ZainoDB,
};

use std::sync::Arc;

#[derive(Clone, Debug)]
/// `DbReader` is the preferred entry point for serving chain queries:
/// - it exposes only read APIs,
/// - it routes each operation via [`CapabilityRequest`] to ensure the selected backend supports the
///   requested feature,
/// - and it remains stable across major migrations because routing is handled internally by the
///   [`Router`](super::router::Router).
///
/// ## Cloning and sharing
/// `DbReader` is cheap to clone; clones share the underlying `Arc<ZainoDB>`.
pub(crate) struct DbReader {
    /// Shared handle to the running `ZainoDB` instance.
    pub(crate) inner: Arc<ZainoDB>,
}

impl DbReader {
    /// Resolves the backend that should serve `cap` right now.
    ///
    /// This is the single routing choke-point for all `DbReader` methods. It delegates to
    /// `ZainoDB::backend_for_cap`, which consults the router’s primary/shadow masks.
    ///
    /// # Errors
    /// Returns `FinalisedStateError::FeatureUnavailable(...)` if no currently-open backend
    /// advertises the requested capability.
    #[inline(always)]
    fn db(&self, cap: CapabilityRequest) -> Result<Arc<DbBackend>, FinalisedStateError> {
        self.inner.backend_for_cap(cap)
    }

    // ***** DB Core Read *****

    /// Returns the current runtime status of the serving database.
    ///
    /// This reflects the status of the backend currently serving `READ_CORE`, which is the minimum
    /// capability required for basic chain queries.
    pub(crate) fn status(&self) -> StatusType {
        self.inner.status()
    }

    /// Returns the greatest block `Height` stored in the database, or `None` if the DB is empty.
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.inner.db_height().await
    }

    /// Fetches the persisted database metadata singleton (`DbMetadata`).
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.inner.get_metadata().await
    }

    /// Waits until the database reports [`StatusType::Ready`].
    ///
    /// This is a convenience wrapper around `ZainoDB::wait_until_ready` and should typically be
    /// awaited once during startup before serving queries.
    pub(crate) async fn wait_until_ready(&self) {
        self.inner.wait_until_ready().await
    }

    /// Fetches the main-chain height for a given block hash, if present in finalised state.
    pub(crate) async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.inner.get_block_height(hash).await
    }

    /// Fetches the main-chain block hash for a given block height, if present in finalised state.
    pub(crate) async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        self.inner.get_block_hash(height).await
    }

    // ***** Block Core Ext *****

    /// Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    pub(crate) async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_tx_location(txid)
            .await
    }

    /// Fetch block header data by height.
    pub(crate) async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_block_header(height)
            .await
    }

    /// Fetches block headers for the given height range.
    pub(crate) async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_block_range_headers(start, end)
            .await
    }

    /// Fetch the txid bytes for a given TxLocation.
    pub(crate) async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_txid(tx_location)
            .await
    }

    /// Fetch block txids by height.
    pub(crate) async fn get_block_txids(
        &self,
        height: Height,
    ) -> Result<TxidList, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_block_txids(height)
            .await
    }

    /// Fetches block txids for the given height range.
    pub(crate) async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockCoreExt)?
            .get_block_range_txids(start, end)
            .await
    }

    // ***** Block Transparent Ext *****

    /// Fetch the serialized TransparentCompactTx for the given TxLocation, if present.
    pub(crate) async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockTransparentExt)?
            .get_transparent(tx_location)
            .await
    }

    /// Fetch block transparent transaction data by height.
    pub(crate) async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        self.db(CapabilityRequest::BlockTransparentExt)?
            .get_block_transparent(height)
            .await
    }

    /// Fetches block transparent tx data for the given height range.
    pub(crate) async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockTransparentExt)?
            .get_block_range_transparent(start, end)
            .await
    }

    // ***** Block shielded Ext *****

    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    pub(crate) async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_sapling(tx_location)
            .await
    }

    /// Fetch block sapling transaction data by height.
    pub(crate) async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_block_sapling(height)
            .await
    }

    /// Fetches block sapling tx data for the given height range.
    pub(crate) async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_block_range_sapling(start, end)
            .await
    }

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    pub(crate) async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_orchard(tx_location)
            .await
    }

    /// Fetch block orchard transaction data by height.
    pub(crate) async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_block_orchard(height)
            .await
    }

    /// Fetches block orchard tx data for the given height range.
    pub(crate) async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_block_range_orchard(start, end)
            .await
    }

    /// Fetch block commitment tree data by height.
    pub(crate) async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
            .get_block_commitment_tree_data(height)
            .await
    }

    /// Fetches block commitment tree data for the given height range.
    pub(crate) async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        self.db(CapabilityRequest::BlockShieldedExt)?
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
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
            .addr_records(addr_script)
            .await
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
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
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
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
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
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
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
    ) -> Result<i64, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
            .addr_balance_by_range(addr_script, start_height, end_height)
            .await
    }

    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
            .get_outpoint_spender(outpoint)
            .await
    }

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        self.db(CapabilityRequest::TransparentHistExt)?
            .get_outpoint_spenders(outpoints)
            .await
    }

    // ***** IndexedBlock Ext *****

    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    pub(crate) async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        self.db(CapabilityRequest::IndexedBlockExt)?
            .get_chain_block(height)
            .await
    }

    // ***** CompactBlock Ext *****

    /// Returns the CompactBlock for the given Height.
    pub(crate) async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.db(CapabilityRequest::CompactBlockExt)?
            .get_compact_block(height, pool_types)
            .await
    }

    pub(crate) async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        self.db(CapabilityRequest::CompactBlockExt)?
            .get_compact_block_stream(start_height, end_height, pool_types)
            .await
    }
}
