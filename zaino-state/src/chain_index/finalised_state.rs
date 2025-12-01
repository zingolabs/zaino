//! Holds the Finalised portion of the chain index on disk.

// TODO / FIX - REMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod db;
pub(crate) mod entry;
pub(crate) mod migrations;
pub(crate) mod reader;
pub(crate) mod router;

use capability::*;
use db::{DbBackend, VERSION_DIRS};
use migrations::MigrationManager;
use reader::*;
use router::Router;
use tracing::info;
use zebra_chain::parameters::NetworkKind;

use crate::{
    chain_index::{source::BlockchainSourceError, types::GENESIS_HEIGHT},
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock, StatusType,
};

use std::{sync::Arc, time::Duration};
use tokio::time::{interval, MissedTickBehavior};

use super::source::BlockchainSource;

pub(crate) struct ZainoDB {
    db: Arc<Router>,
    cfg: BlockCacheConfig,
}

impl ZainoDB {
    // ***** DB control *****

    /// Spawns a ZainoDB, opens an existing database if a path is given in the config else creates a new db.
    ///
    /// Peeks at the db metadata store to load correct database version.
    pub(crate) async fn spawn<T>(
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<Self, FinalisedStateError>
    where
        T: BlockchainSource,
    {
        let version_opt = Self::try_find_current_db_version(&cfg).await;

        let target_version = match cfg.db_version {
            0 => DbVersion {
                major: 0,
                minor: 0,
                patch: 0,
            },
            1 => DbVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            x => {
                return Err(FinalisedStateError::Custom(format!(
                    "unsupported database version: DbV{x}"
                )));
            }
        };

        let backend = match version_opt {
            Some(version) => {
                info!("Opening ZainoDBv{} from file.", version);
                match version {
                    0 => DbBackend::spawn_v0(&cfg).await?,
                    1 => DbBackend::spawn_v1(&cfg).await?,
                    _ => {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported database version: DbV{version}"
                        )));
                    }
                }
            }
            None => {
                info!("Creating new ZainoDBv{}.", target_version);
                match target_version.major() {
                    0 => DbBackend::spawn_v0(&cfg).await?,
                    1 => DbBackend::spawn_v1(&cfg).await?,
                    _ => {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported database version: DbV{target_version}"
                        )));
                    }
                }
            }
        };
        let current_version = backend.get_metadata().await?.version();

        let router = Arc::new(Router::new(Arc::new(backend)));

        if version_opt.is_some() && current_version < target_version {
            info!(
                "Starting ZainoDB migration manager, migratiing database from v{} to v{}.",
                current_version, target_version
            );
            let mut migration_manager = MigrationManager {
                router: Arc::clone(&router),
                cfg: cfg.clone(),
                current_version,
                target_version,
                source,
            };
            migration_manager.migrate().await?;
        }

        Ok(Self { db: router, cfg })
    }

    /// Gracefully shuts down the running ZainoDB, closing all child processes.
    pub(crate) async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.db.shutdown().await
    }

    /// Returns the status of the running ZainoDB.
    pub(crate) fn status(&self) -> StatusType {
        self.db.status()
    }

    /// Waits until the ZainoDB returns a Ready status.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if self.db.status() == StatusType::Ready {
                break;
            }
        }
    }

    /// Creates a read-only viewer onto the running ZainoDB.
    ///
    /// NOTE: **ALL** chain fetch should use DbReader instead of directly using ZainoDB.
    pub(crate) fn to_reader(self: &Arc<Self>) -> DbReader {
        DbReader {
            inner: Arc::clone(self),
        }
    }

    /// Look for known dirs to find current db version.
    ///
    /// The oldest version is returned as the database may have been closed mid migration.
    ///
    /// * `Some(version)` – DB exists, version returned.
    /// * `None`      – directory or key is missing -> fresh DB.
    async fn try_find_current_db_version(cfg: &BlockCacheConfig) -> Option<u32> {
        let legacy_dir = match cfg.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "live",
            NetworkKind::Testnet => "test",
            NetworkKind::Regtest => "local",
        };
        let legacy_path = cfg.storage.database.path.join(legacy_dir);
        if legacy_path.join("data.mdb").exists() && legacy_path.join("lock.mdb").exists() {
            return Some(0);
        }

        let net_dir = match cfg.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "mainnet",
            NetworkKind::Testnet => "testnet",
            NetworkKind::Regtest => "regtest",
        };
        let net_path = cfg.storage.database.path.join(net_dir);
        if net_path.exists() && net_path.is_dir() {
            for (i, version_dir) in VERSION_DIRS.iter().enumerate() {
                let db_path = net_path.join(version_dir);
                let data_file = db_path.join("data.mdb");
                let lock_file = db_path.join("lock.mdb");
                if data_file.exists() && lock_file.exists() {
                    let version = (i + 1) as u32;
                    return Some(version);
                }
            }
        }

        None
    }

    /// Returns the internal db backend for the given db capability.
    ///
    /// Used by DbReader to route calls to the correct database during major migrations.
    #[inline]
    pub(crate) fn backend_for_cap(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<DbBackend>, FinalisedStateError> {
        self.db.backend(cap)
    }

    // ***** Db Core Write *****

    /// Sync the database to the given height using the given BlockchainSource.
    pub(crate) async fn sync_to_height<T>(
        &self,
        height: Height,
        source: T,
    ) -> Result<(), FinalisedStateError>
    where
        T: BlockchainSource,
    {
        let network = self.cfg.network;
        let db_height_opt = self.db_height().await?;
        let mut db_height = db_height_opt.unwrap_or(GENESIS_HEIGHT);

        let mut parent_chainwork = if db_height_opt.is_none() {
            ChainWork::from_u256(0.into())
        } else {
            db_height.0 += 1;
            match self
                .db
                .backend(CapabilityRequest::BlockCoreExt)?
                .get_block_header(height)
                .await
            {
                Ok(header) => *header.index().chainwork(),
                // V0 does not hold or use chainwork, and does not serve header data,
                // can we handle this better?
                //
                // can we get this data from zebra blocks?
                Err(_) => ChainWork::from_u256(0.into()),
            }
        };

        for height_int in (db_height.0)..=height.0 {
            let block = match source
                .get_block(zebra_state::HashOrHeight::Height(
                    zebra_chain::block::Height(height_int),
                ))
                .await?
            {
                Some(block) => block,
                None => {
                    return Err(FinalisedStateError::BlockchainSourceError(
                        BlockchainSourceError::Unrecoverable(format!(
                            "error fetching block at height {} from validator",
                            height.0
                        )),
                    ));
                }
            };

            let block_hash = BlockHash::from(block.hash().0);

            let (sapling_root, sapling_size, orchard_root, orchard_size) =
                match source.get_commitment_tree_roots(block_hash).await? {
                    (Some((sapling_root, sapling_size)), Some((orchard_root, orchard_size))) => {
                        (sapling_root, sapling_size, orchard_root, orchard_size)
                    }
                    (None, _) => {
                        return Err(FinalisedStateError::BlockchainSourceError(
                            BlockchainSourceError::Unrecoverable(format!(
                                "missing Sapling commitment tree root for block {block_hash}"
                            )),
                        ));
                    }
                    (_, None) => {
                        return Err(FinalisedStateError::BlockchainSourceError(
                            BlockchainSourceError::Unrecoverable(format!(
                                "missing Orchard commitment tree root for block {block_hash}"
                            )),
                        ));
                    }
                };

            let metadata = BlockMetadata::new(
                sapling_root,
                sapling_size as u32,
                orchard_root,
                orchard_size as u32,
                parent_chainwork,
                network.to_zebra_network(),
            );

            let block_with_metadata = BlockWithMetadata::new(block.as_ref(), metadata);
            let chain_block = match IndexedBlock::try_from(block_with_metadata) {
                Ok(block) => block,
                Err(_) => {
                    return Err(FinalisedStateError::BlockchainSourceError(
                        BlockchainSourceError::Unrecoverable(format!(
                            "error building block data at height {}",
                            height.0
                        )),
                    ));
                }
            };
            parent_chainwork = *chain_block.index().chainwork();

            self.write_block(chain_block).await?;
        }

        Ok(())
    }

    /// Writes a block to the database.
    ///
    /// This **MUST** be the *next* block in the chain (db_tip_height + 1).
    pub(crate) async fn write_block(&self, b: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.db.write_block(b).await
    }

    /// Deletes a block from the database by height.
    ///
    /// This **MUST** be the *top* block in the db.
    ///
    /// Uses `delete_block` internally, fails if the block to be deleted cannot be correctly built.
    /// If this happens, the block to be deleted must be fetched from the validator and given to `delete_block`
    /// to ensure the block has been completely wiped from the database.
    pub(crate) async fn delete_block_at_height(
        &self,
        h: Height,
    ) -> Result<(), FinalisedStateError> {
        self.db.delete_block_at_height(h).await
    }

    /// Deletes a given block from the database.
    ///
    /// This **MUST** be the *top* block in the db.
    pub(crate) async fn delete_block(&self, b: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.db.delete_block(b).await
    }

    // ***** DB Core Read *****

    /// Returns the highest block height held in the database.
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.db.db_height().await
    }

    /// Returns the block height for the given block hash *if* present in the finalised state.
    pub(crate) async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.db.get_block_height(hash).await
    }

    /// Returns the block block hash for the given block height *if* present in the finlaised state.
    pub(crate) async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        self.db.get_block_hash(height).await
    }

    /// Returns metadata for the running ZainoDB.
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.db.get_metadata().await
    }

    #[cfg(test)]
    pub(crate) fn router(&self) -> &Router {
        &self.db
    }
}
