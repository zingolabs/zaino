//! Migration management and implementations.

use super::{
    capability::{
        BlockCoreExt, Capability, DbCore as _, DbRead, DbVersion, DbWrite, MigrationStatus,
    },
    db::DbBackend,
    router::Router,
};

use crate::{
    chain_index::{source::BlockchainSource, types::GENESIS_HEIGHT},
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock,
};

use async_trait::async_trait;
use std::sync::Arc;
use tracing::info;
use zebra_chain::parameters::NetworkKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationType {
    Patch,
    Minor,
    Major,
}

#[async_trait]
pub trait Migration<T: BlockchainSource> {
    const CURRENT_VERSION: DbVersion;
    const TO_VERSION: DbVersion;

    fn current_version(&self) -> DbVersion {
        Self::CURRENT_VERSION
    }

    fn to_version(&self) -> DbVersion {
        Self::TO_VERSION
    }

    async fn migrate(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<(), FinalisedStateError>;
}

pub(super) struct MigrationManager<T: BlockchainSource> {
    pub(super) router: Arc<Router>,
    pub(super) cfg: BlockCacheConfig,
    pub(super) current_version: DbVersion,
    pub(super) target_version: DbVersion,
    pub(super) source: T,
}

impl<T: BlockchainSource> MigrationManager<T> {
    /// Iteratively performs each migration step from current version to target version.
    pub(super) async fn migrate(&mut self) -> Result<(), FinalisedStateError> {
        while self.current_version < self.target_version {
            let migration = self.get_migration()?;
            migration
                .migrate(
                    Arc::clone(&self.router),
                    self.cfg.clone(),
                    self.source.clone(),
                )
                .await?;
            self.current_version = migration.to_version();
        }

        Ok(())
    }

    /// Return the next migration for the current version.
    fn get_migration(&self) -> Result<impl Migration<T>, FinalisedStateError> {
        match (
            self.current_version.major,
            self.current_version.minor,
            self.current_version.patch,
        ) {
            (0, 0, 0) => Ok(Migration0_0_0To1_0_0),
            (_, _, _) => Err(FinalisedStateError::Custom(format!(
                "Missing migration from version {}",
                self.current_version
            ))),
        }
    }
}

// ***** Migrations *****

struct Migration0_0_0To1_0_0;

#[async_trait]
impl<T: BlockchainSource> Migration<T> for Migration0_0_0To1_0_0 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 0,
        minor: 0,
        patch: 0,
    };
    const TO_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 0,
        patch: 0,
    };

    /// The V0 database that we are migrating from was a lightwallet specific database
    /// that only built compact block data from sapling activation onwards.
    /// DbV1 is required to be built from genasis to correctly build the transparent address indexes.
    /// For this reason we do not do any partial builds in the V0 to V1 migration.
    /// We just run V0 as primary until V1 is fully built in shadow, then switch primary, deleting V0.
    async fn migrate(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        info!("Starting v0.0.0 to v1.0.0 migration.");
        // Open V1 as shadow
        let shadow = Arc::new(DbBackend::spawn_v1(&cfg).await?);
        router.set_shadow(Arc::clone(&shadow), Capability::empty());

        let migration_status = shadow.get_metadata().await?.migration_status();

        match migration_status {
            MigrationStatus::Empty
            | MigrationStatus::PartialBuidInProgress
            | MigrationStatus::PartialBuildComplete
            | MigrationStatus::FinalBuildInProgress => {
                // build shadow to primary_db_height,
                // start from shadow_db_height in case database was shutdown mid-migration.
                let mut parent_chain_work = ChainWork::from_u256(0.into());

                let shadow_db_height_opt = shadow.db_height().await?;
                let mut shadow_db_height = shadow_db_height_opt.unwrap_or(GENESIS_HEIGHT);
                let mut build_start_height = if shadow_db_height_opt.is_some() {
                    parent_chain_work = *shadow
                        .get_block_header(shadow_db_height)
                        .await?
                        .index()
                        .chainwork();

                    shadow_db_height + 1
                } else {
                    shadow_db_height
                };
                let mut primary_db_height = router.db_height().await?.unwrap_or(GENESIS_HEIGHT);

                info!(
                    "Starting shadow database build, current database tips: v0:{} v1:{}",
                    primary_db_height, shadow_db_height
                );

                loop {
                    if shadow_db_height >= primary_db_height {
                        break;
                    }

                    for height in (build_start_height.0)..=primary_db_height.0 {
                        let block = source
                            .get_block(zebra_state::HashOrHeight::Height(
                                zebra_chain::block::Height(height),
                            ))
                            .await?
                            .ok_or_else(|| {
                                FinalisedStateError::Custom(format!(
                                    "block not found at height {height}"
                                ))
                            })?;
                        let hash = BlockHash::from(block.hash().0);

                        let (sapling_root_data, orchard_root_data) =
                            source.get_commitment_tree_roots(hash).await?;
                        let (sapling_root, sapling_root_size) =
                            sapling_root_data.ok_or_else(|| {
                                FinalisedStateError::Custom(format!(
                        "sapling commitment tree data missing for block {hash:?} at height {height}"
                    ))
                            })?;
                        let (orchard_root, orchard_root_size) =
                            orchard_root_data.ok_or_else(|| {
                                FinalisedStateError::Custom(format!(
                        "orchard commitment tree data missing for block {hash:?} at height {height}"
                    ))
                            })?;

                        let metadata = BlockMetadata::new(
                            sapling_root,
                            sapling_root_size as u32,
                            orchard_root,
                            orchard_root_size as u32,
                            parent_chain_work,
                            cfg.network.to_zebra_network(),
                        );

                        let block_with_metadata = BlockWithMetadata::new(block.as_ref(), metadata);
                        let chain_block =
                            IndexedBlock::try_from(block_with_metadata).map_err(|_| {
                                FinalisedStateError::Custom(
                                    "Failed to build chain block".to_string(),
                                )
                            })?;

                        parent_chain_work = *chain_block.chainwork();

                        shadow.write_block(chain_block).await?;
                    }

                    std::thread::sleep(std::time::Duration::from_millis(100));

                    shadow_db_height = shadow.db_height().await?.unwrap_or(Height(0));
                    build_start_height = shadow_db_height + 1;
                    primary_db_height = router.db_height().await?.unwrap_or(Height(0));
                }

                // update db metadata migration status
                let mut metadata = shadow.get_metadata().await?;
                metadata.migration_status = MigrationStatus::Complete;
                shadow.update_metadata(metadata).await?;

                info!("v1 database build complete.");
            }

            MigrationStatus::Complete => {
                // Migration complete, continue with DbV0 deletion.
            }
        }

        info!("promoting v1 database to primary.");

        // Promote V1 to primary
        let db_v0 = router.promote_shadow()?;

        // Delete V0
        tokio::spawn(async move {
            // Wait until all Arc<DbBackend> clones are dropped
            while Arc::strong_count(&db_v0) > 1 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            // shutdown database
            if let Err(e) = db_v0.shutdown().await {
                tracing::warn!("Old primary shutdown failed: {e}");
            }

            // Now safe to delete old database files
            let db_path_dir = match cfg.network.to_zebra_network().kind() {
                NetworkKind::Mainnet => "live",
                NetworkKind::Testnet => "test",
                NetworkKind::Regtest => "local",
            };
            let db_path = cfg.storage.database.path.join(db_path_dir);

            info!("Wiping v0 database from disk.");

            match tokio::fs::remove_dir_all(&db_path).await {
                Ok(_) => tracing::info!("Deleted old database at {}", db_path.display()),
                Err(e) => tracing::error!(
                    "Failed to delete old database at {}: {}",
                    db_path.display(),
                    e
                ),
            }
        });

        info!("v0.0.0 to v1.0.0 migration complete.");

        Ok(())
    }
}
