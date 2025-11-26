//! Holds Database implementations by *major* version.

pub(crate) mod v0;
pub(crate) mod v1;

use v0::DbV0;
use v1::DbV1;

use crate::{
    chain_index::{
        finalized_state::capability::{
            BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbCore,
            DbMetadata, DbRead, DbWrite, IndexedBlockExt, TransparentHistExt,
        },
        types::TransactionHash,
    },
    config::BlockCacheConfig,
    error::FinalizedStateError,
    AddrScript, BlockHash, BlockHeaderData, CommitmentTreeData, Height, IndexedBlock,
    OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList, StatusType,
    TransparentCompactTx, TransparentTxList, TxLocation, TxidList,
};

use async_trait::async_trait;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use super::capability::Capability;

/// New versions must be also be appended to this list and there must be no missing versions for correct functionality.
pub(super) const VERSION_DIRS: [&str; 1] = ["v1"];

/// All concrete database implementations.
pub(crate) enum DbBackend {
    V0(DbV0),
    V1(DbV1),
}

// ***** Core database functionality *****

impl DbBackend {
    /// Spawn a v0 database.
    pub(crate) async fn spawn_v0(cfg: &BlockCacheConfig) -> Result<Self, FinalizedStateError> {
        Ok(Self::V0(DbV0::spawn(cfg).await?))
    }

    /// Spawn a v1 database.
    pub(crate) async fn spawn_v1(cfg: &BlockCacheConfig) -> Result<Self, FinalizedStateError> {
        Ok(Self::V1(DbV1::spawn(cfg).await?))
    }

    /// Waits until the ZainoDB returns a Ready status.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if self.status() == StatusType::Ready {
                break;
            }
        }
    }

    /// Returns the capabilities supported by this database instance.
    pub(crate) fn capability(&self) -> Capability {
        match self {
            Self::V0(_) => {
                Capability::READ_CORE | Capability::WRITE_CORE | Capability::COMPACT_BLOCK_EXT
            }
            Self::V1(_) => Capability::LATEST,
        }
    }
}

impl From<DbV0> for DbBackend {
    fn from(value: DbV0) -> Self {
        Self::V0(value)
    }
}

impl From<DbV1> for DbBackend {
    fn from(value: DbV1) -> Self {
        Self::V1(value)
    }
}

#[async_trait]
impl DbCore for DbBackend {
    fn status(&self) -> StatusType {
        match self {
            // TODO private
            Self::V0(db) => db.status(),
            Self::V1(db) => db.status(),
        }
    }

    async fn shutdown(&self) -> Result<(), FinalizedStateError> {
        match self {
            Self::V0(db) => db.shutdown().await,
            Self::V1(db) => db.shutdown().await,
        }
    }
}

#[async_trait]
impl DbRead for DbBackend {
    async fn db_height(&self) -> Result<Option<Height>, FinalizedStateError> {
        match self {
            Self::V0(db) => db.db_height().await,
            Self::V1(db) => db.db_height().await,
        }
    }

    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalizedStateError> {
        match self {
            Self::V0(db) => db.get_block_height(hash).await,
            Self::V1(db) => db.get_block_height(hash).await,
        }
    }

    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalizedStateError> {
        match self {
            Self::V0(db) => db.get_block_hash(height).await,
            Self::V1(db) => db.get_block_hash(height).await,
        }
    }

    async fn get_metadata(&self) -> Result<DbMetadata, FinalizedStateError> {
        match self {
            Self::V0(db) => db.get_metadata().await,
            Self::V1(db) => db.get_metadata().await,
        }
    }
}

#[async_trait]
impl DbWrite for DbBackend {
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalizedStateError> {
        match self {
            Self::V0(db) => db.write_block(block).await,
            Self::V1(db) => db.write_block(block).await,
        }
    }

    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalizedStateError> {
        match self {
            Self::V0(db) => db.delete_block_at_height(height).await,
            Self::V1(db) => db.delete_block_at_height(height).await,
        }
    }

    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalizedStateError> {
        match self {
            Self::V0(db) => db.delete_block(block).await,
            Self::V1(db) => db.delete_block(block).await,
        }
    }

    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalizedStateError> {
        match self {
            Self::V0(db) => db.update_metadata(metadata).await,
            Self::V1(db) => db.update_metadata(metadata).await,
        }
    }
}

// ***** Database capability extension traits *****

#[async_trait]
impl BlockCoreExt for DbBackend {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_header(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_headers(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_txids(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_txids(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_txid(tx_location).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_tx_location(txid).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_core")),
        }
    }
}

#[async_trait]
impl BlockTransparentExt for DbBackend {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_transparent(tx_location).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_transparent(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_transparent(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_transparent")),
        }
    }
}

#[async_trait]
impl BlockShieldedExt for DbBackend {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_sapling(tx_location).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_sapling(&self, h: Height) -> Result<SaplingTxList, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_sapling(h).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_sapling(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_orchard(tx_location).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_orchard(&self, h: Height) -> Result<OrchardTxList, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_orchard(h).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_orchard(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_commitment_tree_data(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_commitment_tree_data(start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("block_shielded")),
        }
    }
}

#[async_trait]
impl CompactBlockExt for DbBackend {
    async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalizedStateError> {
        #[allow(unreachable_patterns)]
        match self {
            Self::V0(db) => db.get_compact_block(height).await,
            Self::V1(db) => db.get_compact_block(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("compact_block")),
        }
    }
}

#[async_trait]
impl IndexedBlockExt for DbBackend {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_chain_block(height).await,
            _ => Err(FinalizedStateError::FeatureUnavailable("chain_block")),
        }
    }
}

#[async_trait]
impl TransparentHistExt for DbBackend {
    async fn addr_records(
        &self,
        script: AddrScript,
    ) -> Result<Option<Vec<crate::chain_index::types::AddrEventBytes>>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.addr_records(script).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn addr_and_index_records(
        &self,
        script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<crate::chain_index::types::AddrEventBytes>>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.addr_and_index_records(script, tx_location).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn addr_tx_locations_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.addr_tx_locations_by_range(script, start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn addr_utxos_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.addr_utxos_by_range(script, start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn addr_balance_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<i64, FinalizedStateError> {
        match self {
            Self::V1(db) => db.addr_balance_by_range(script, start, end).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_outpoint_spender(outpoint).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalizedStateError> {
        match self {
            Self::V1(db) => db.get_outpoint_spenders(outpoints).await,
            _ => Err(FinalizedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }
}
