//! ZainoDbReader: Read only view onto a running ZainoDB
//!
//! This should be used to fetch chain data in *all* cases.

use crate::{
    chain_index::{
        finalised_state::capability::CapabilityRequest,
        types::{AddrEventBytes, TransactionHash},
    },
    error::FinalisedStateError,
    AddrScript, BlockHash, BlockHeaderData, CommitmentTreeData, Height, IndexedBlock,
    OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList, StatusType,
    TransparentCompactTx, TransparentTxList, TxLocation, TxidList,
};

use super::{
    capability::{
        BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbMetadata,
        IndexedBlockExt, TransparentHistExt,
    },
    db::DbBackend,
    ZainoDB,
};

use std::sync::Arc;

/// Immutable view onto an already-running [`ZainoDB`].
///
/// Carries a plain reference with the same lifetime as the parent DB
#[derive(Clone)]
pub(crate) struct DbReader {
    /// Immutable read-only view onto the running ZainoDB
    pub(crate) inner: Arc<ZainoDB>,
}

impl DbReader {
    /// Returns the internal db backend for the given db capability.
    #[inline(always)]
    fn db(&self, cap: CapabilityRequest) -> Result<Arc<DbBackend>, FinalisedStateError> {
        self.inner.backend_for_cap(cap)
    }
    // ***** DB Core Read *****

    /// Returns the status of the serving ZainoDB.
    pub(crate) fn status(&self) -> StatusType {
        self.inner.status()
    }

    /// Returns the greatest block `Height` stored in the db
    /// (`None` if the DB is still empty).
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.inner.db_height().await
    }

    /// Fetch database metadata.
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.inner.get_metadata().await
    }

    /// Awaits untile the DB returns a Ready status.
    pub(crate) async fn wait_until_ready(&self) {
        self.inner.wait_until_ready().await
    }

    /// Fetch the block height in the main chain for a given block hash.
    pub(crate) async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.inner.get_block_height(hash).await
    }

    /// Fetch the block hash in the main chain for a given block height.
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
    ///
    /// TODO: Add separate range fetch method!
    pub(crate) async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.db(CapabilityRequest::CompactBlockExt)?
            .get_compact_block(height)
            .await
    }
}
