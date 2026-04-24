//! Database version migration framework and implementations
//!
//! This file defines how `ZainoDB` migrates on-disk databases between database versions.
//!
//! Migrations are orchestrated by [`MigrationManager`], which is invoked from `ZainoDB::spawn` when
//! `current_version < target_version`.
//!
//! The migration model is **stepwise**:
//! - each migration maps one concrete `DbVersion` to the next supported `DbVersion`,
//! - the manager iteratively applies steps until the target is reached.
//!
//! # Key concepts
//!
//! - [`Migration<T>`] trait:
//!   - declares `CURRENT_VERSION` and `TO_VERSION` constants,
//!   - provides an async `migrate(...)` entry point.
//!
//! - [`MigrationManager<T>`]:
//!   - holds the router, config, current and target versions, and a `BlockchainSource`,
//!   - repeatedly selects and runs the next migration via `get_migration()`.
//!
//! - [`MigrationStep`]:
//!   - enum-based dispatch wrapper used by `MigrationManager` to select between multiple concrete
//!     `Migration<T>` implementations (Rust cannot return different `impl Trait` types from a `match`).
//!
//! - [`capability::MigrationStatus`]:
//!   - stored in `DbMetadata` and used to resume work safely after shutdown.
//!
//! # How major migrations work in this codebase
//!
//! This module is designed around the router’s **primary + shadow** model:
//!
//! - The *primary* DB continues serving read/write traffic.
//! - A *shadow* DB (new schema version) is created and built in parallel.
//! - Once the shadow DB is fully built and marked complete, it is promoted to primary.
//! - The old primary DB is shut down and deleted from disk once all handles are dropped.
//!
//! This minimises downtime and allows migrations that require a full rebuild (rather than an
//! in-place rewrite) without duplicating the entire DB indefinitely.
//!
//! It ia also possible (if migration allows) to partially build the new database version, switch
//! specific functionality to the shadow, and partialy delete old the database version, rather than
//! building the new database in full. This enables developers to minimise transient disk usage
//! during migrations.
//!
//! # Notes on MigrationType
//!
//! Database versioning (and migration) is split into three distinct types, dependant of the severity
//! of changes being made to the database:
//! - Major versions / migrations:
//!   - Major schema / capability changes, notably changes that require refetching the complete
//!     blockchain from the backing validator / finaliser to build / update database indices.
//!   - Migrations should follow the "primary" database / "shadow" database model. The legacy database
//!     should be spawned as the "primary" and set to carry on serving data during migration. The new
//!     database version is then spawned as the "shadow" and built in a background process. Once the
//!     "shadow" is built to "primary" db tip height it is promoted to primary, taking over serving
//!     data from the legacy database, the demoted database can then be safely removed from disk. It is
//!     also possible to partially build the new database version , promote specific database capability,
//!     and delete specific tables from the legacy database, reducing transient disk usage.
//! - Minor versions / migrations:
//!   - Updates involving minor schema / capability changes, notably changes that can be rebuilt in place
//!     (changes that do not require fetching new data from the backing validator / finaliser) or that can
//!     rely on updates to the versioned serialisation / deserialisation of database structures.
//!   - Migrations for minor patch bumps can follow several paths. If the database table being updated
//!     holds variable length items, and the actual data being held is not changed (only format changes
//!     being applied) then it may be possible to rely on serialisation / deserialisation updates to the
//!     items being chenged, with the database table holding a mix of serialisation versions. However,
//!     if the table being updated is of fixed length items, or the actual data held is being updated,
//!     then it will be necessary to rebuild that table in full, possibly requiring database downtime for
//!     the migration. Since this only involves moving data already held in the database (rather than
//!     fetching new data from the backing validator) migration should be quick and short downtimes are
//!     accepted.
//! - Patch versions / migrations:
//!   - Changes to database code that do not touch the database schema, these include bug fixes,
//!     performance improvements etc.
//!   - Migrations for patch updates only need to handle updating the stored DbMetadata singleton.
//!
//! # Development: adding a new migration step
//!
//! 1. Introduce a new `struct MigrationX_Y_ZToA_B_C;` and implement `Migration<T>`.
//! 2. Add a new `MigrationStep` variant and register it in `MigrationManager::get_migration()` by
//!    matching on the *current* version.
//! 3. Ensure the migration is:
//!    - deterministic,
//!    - resumable (use `DbMetadata::migration_status` and/or shadow tip),
//!    - crash-safe (never leaves a partially promoted DB).
//! 4. Add tests/fixtures for:
//!    - starting from the old version,
//!    - resuming mid-build if applicable,
//!    - validating the promoted DB serves required capabilities.
//!
//! # Implemented migrations
//!
//! ## v0.0.0 → v1.0.0
//!
//! `Migration0_0_0To1_0_0` performs a **full shadow rebuild from genesis**.
//!
//! Rationale (as enforced by code/comments):
//! - The legacy v0 DB is a lightwallet-specific store that only builds compact blocks from Sapling
//!   activation onwards.
//! - v1 requires data from genesis (notably for transparent address history indices), therefore a
//!   partial “continue from Sapling” build is insufficient.
//!
//! Mechanics:
//! - Spawn v1 as a shadow backend.
//! - Determine the current shadow tip (to resume if interrupted).
//! - Fetch blocks and commitment tree roots from the `BlockchainSource` starting at either genesis
//!   or `shadow_tip + 1`, building `BlockMetadata` and `IndexedBlock`.
//! - Keep building until the shadow catches up to the primary tip (looping because the primary can
//!   advance during the build).
//! - Mark `migration_status = Complete` in shadow metadata.
//! - Promote shadow to primary via `router.promote_shadow()`.
//! - Delete the old v0 directory asynchronously once all strong references are dropped.
//!
//! ## v1.0.0 → v1.1.0
//!
//! `Migration1_0_0To1_1_0` is a **minor version bump** with **on disk schema changes**, but does
//! not include changes to the external ZainoDB API.
//!
//! Important changes in v1.1.0:
//! - ZainoVersionedSerde had a bug which stopped varifying the checksum of older serde formats,
//!   this meant that is was not possible to safely update database formats without a full DB
//!   rebuild. This bug has been fixed and all serde updated to follow the new contract (Note this
//!   change is 100% compaitible with the old sschema, only extending functionality as required).
//! - BlockHeaderData v2 added: the Height field in BlockHeaderData.BlockIndex is no longer
//!   optional. (Note, as heights are required for the finalised portion of the chain this does not
//!   change db logic, as height was already gruenteed, with a error returned if a block with no
//!   height is every written to the db).
//!
//! Important note: `BlockHeaderData` now has a V2 on-disk layout which uses the V2
//! `BlockIndex` wire format. Because the `headers` table stores `BlockHeaderData` as a
//! `StoredEntryVar` (no fixed-length optimisations), the table may contain both V1 and V2
//! `BlockHeaderData` records concurrently. This migration is metadata-only: it advances
//! `DbMetadata::version` and refreshes the recorded schema checksum so persisted metadata
//! matches the repository's updated schema text.

use super::{
    capability::{
        BlockCoreExt, Capability, DbCore as _, DbRead, DbVersion, DbWrite, MigrationStatus,
    },
    db::DbBackend,
    router::Router,
};

use crate::{
    chain_index::{
        finalised_state::capability::DbMetadata, source::BlockchainSource, types::GENESIS_HEIGHT,
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock,
};

use zebra_chain::parameters::NetworkKind;

use async_trait::async_trait;
use std::sync::Arc;
use tracing::info;

/// Broad categorisation of migration severity.
///
/// This enum exists as a design aid to communicate intent and constraints:
/// - **Patch**: code-only changes; schema is unchanged; typically only `DbMetadata` needs updating.
/// - **Minor**: compatible schema / encoding evolution; may require in-place rebuilds of selected tables.
/// - **Major**: capability or schema changes that require rebuilding indices from the backing validator,
///   typically using the router’s primary/shadow model.
///
/// Note: this enum is not currently used to dispatch behaviour in this file; concrete steps are
/// selected by [`MigrationManager::get_migration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationType {
    /// Patch-level changes: no schema change; metadata updates only.
    Patch,

    /// Minor-level changes: compatible schema/encoding changes; may require in-place table rebuild.
    Minor,

    /// Major-level changes: new schema/capabilities; usually requires shadow rebuild and promotion.
    Major,
}

/// A single migration step from one concrete on-disk version to the next.
///
/// Migrations are designed to be **composable** and **stepwise**: each implementation should map a
/// specific `CURRENT_VERSION` to a specific `TO_VERSION`. The manager then iterates until the target
/// version is reached.
///
/// ## Resumability and crash-safety
/// Migration implementations are expected to be resumable where practical. In this codebase, major
/// migrations typically use:
/// - a shadow database that can be incrementally built,
/// - the shadow tip height as an implicit progress marker,
/// - and [`MigrationStatus`] in `DbMetadata` as an explicit progress marker.
///
/// Implementations must never promote a partially-correct database to primary.
#[async_trait]
pub trait Migration<T: BlockchainSource> {
    /// The exact on-disk version this step migrates *from*.
    const CURRENT_VERSION: DbVersion;

    /// The exact on-disk version this step migrates *to*.
    const TO_VERSION: DbVersion;

    /// Returns the version this step migrates *from*.
    fn current_version(&self) -> DbVersion {
        Self::CURRENT_VERSION
    }

    /// Returns the version this step migrates *to*.
    fn to_version(&self) -> DbVersion {
        Self::TO_VERSION
    }

    /// Performs the migration step.
    ///
    /// Implementations may:
    /// - spawn a shadow backend,
    /// - build or rebuild indices,
    /// - update metadata and migration status,
    /// - and promote the shadow backend to primary via the router.
    ///
    /// # Errors
    /// Returns `FinalisedStateError` if the migration cannot proceed safely or deterministically.
    async fn migrate(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<(), FinalisedStateError>;
}

/// Orchestrates a sequence of migration steps until `target_version` is reached.
///
/// `MigrationManager` is constructed by `ZainoDB::spawn` when it detects that the on-disk database
/// is older than the configured target version.
///
/// The manager:
/// - selects the next step based on the current version,
/// - runs it,
/// - then advances `current_version` to the step’s `TO_VERSION` and repeats.
///
/// The router is shared so that migration steps can use the primary/shadow routing model.
pub(super) struct MigrationManager<T: BlockchainSource> {
    /// Router controlling primary/shadow backends and capability routing.
    pub(super) router: Arc<Router>,

    /// Block-cache configuration (paths, network, configured target DB version, etc.).
    pub(super) cfg: BlockCacheConfig,

    /// The on-disk version currently detected/opened.
    pub(super) current_version: DbVersion,

    /// The configured target version to migrate to.
    pub(super) target_version: DbVersion,

    /// Backing data source used to fetch blocks / tree roots for rebuild-style migrations.
    pub(super) source: T,
}

impl<T: BlockchainSource> MigrationManager<T> {
    /// Iteratively performs each migration step from current version to target version.
    ///
    /// The manager applies steps in order, where each step maps one specific `DbVersion` to the next.
    /// The loop terminates once `current_version >= target_version`.
    ///
    /// # Errors
    /// Returns an error if a migration step is missing for the current version, or if any migration
    /// step fails.
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
            self.current_version = migration.to_version::<T>();
        }

        Ok(())
    }

    /// Returns the next migration step for the current on-disk version.
    ///
    /// This must be updated whenever a new supported DB version is introduced. The match is strict:
    /// if a step is missing, migration is aborted rather than attempting an unsafe fallback.
    fn get_migration(&self) -> Result<MigrationStep, FinalisedStateError> {
        match (
            self.current_version.major,
            self.current_version.minor,
            self.current_version.patch,
        ) {
            (0, 0, 0) => Ok(MigrationStep::Migration0_0_0To1_0_0(Migration0_0_0To1_0_0)),
            (1, 0, 0) => Ok(MigrationStep::Migration1_0_0To1_1_0(Migration1_0_0To1_1_0)),
            (_, _, _) => Err(FinalisedStateError::Custom(format!(
                "Missing migration from version {}",
                self.current_version
            ))),
        }
    }
}

/// Concrete migration step selector.
///
/// Rust cannot return `impl Migration<T>` from a `match` that selects between multiple concrete
/// migration types. `MigrationStep` is the enum-based dispatch wrapper used by [`MigrationManager`]
/// to select a step and call `migrate(...)`, and to read the step’s `TO_VERSION`.
enum MigrationStep {
    Migration0_0_0To1_0_0(Migration0_0_0To1_0_0),
    Migration1_0_0To1_1_0(Migration1_0_0To1_1_0),
}

impl MigrationStep {
    fn to_version<T: BlockchainSource>(&self) -> DbVersion {
        match self {
            MigrationStep::Migration0_0_0To1_0_0(_step) => {
                <Migration0_0_0To1_0_0 as Migration<T>>::TO_VERSION
            }
            MigrationStep::Migration1_0_0To1_1_0(_step) => {
                <Migration1_0_0To1_1_0 as Migration<T>>::TO_VERSION
            }
        }
    }

    async fn migrate<T: BlockchainSource>(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        match self {
            MigrationStep::Migration0_0_0To1_0_0(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_0_0To1_1_0(step) => step.migrate(router, cfg, source).await,
        }
    }
}

// ***** Migrations *****

/// Major migration: v0.0.0 → v1.0.0.
///
/// This migration performs a shadow rebuild of the v1 database from genesis, then promotes the
/// completed shadow to primary and schedules deletion of the old v0 database directory once all
/// handles are dropped.
///
/// See the module-level documentation for the detailed rationale and mechanics.
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

    /// Performs the v0 → v1 major migration using the router’s primary/shadow model.
    ///
    /// The legacy v0 database only supports compact block data from Sapling activation onwards.
    /// DbV1 requires a complete rebuild from genesis to correctly build indices (notably transparent
    /// address history). For this reason, this migration does not attempt partial incremental builds
    /// from Sapling; it rebuilds v1 in full in a shadow backend, then promotes it.
    ///
    /// ## Resumption behaviour
    /// If the process is shut down mid-migration:
    /// - the v1 shadow DB directory may already exist,
    /// - shadow tip height is used to resume from `shadow_tip + 1`,
    /// - and `MigrationStatus` is used as a coarse progress marker.
    ///
    /// Promotion occurs only after the v1 build loop has caught up to the primary tip and the shadow
    /// metadata is marked `Complete`.
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
                    parent_chain_work = shadow
                        .get_block_header(shadow_db_height)
                        .await?
                        .context
                        .chainwork;

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

/// Minor migration: v1.0.0 → v1.1.0.
///
/// Important note: `BlockHeaderData` now has a V2 on-disk layout which uses the V2
/// `BlockIndex` wire format. Because the `headers` table stores `BlockHeaderData` as a
/// `StoredEntryVar` (no fixed-length optimisations), the table may contain both V1 and V2
/// `BlockHeaderData` records concurrently. This migration is metadata-only: it advances
/// `DbMetadata::version` and refreshes the recorded schema checksum so persisted metadata
/// matches the repository's updated schema text.
///
/// Safety and resumability:
/// - Idempotent: if run more than once, it will re-write the same metadata.
/// - No shadow database and no table rebuild.
/// - Clears any stale in-progress migration status.
struct Migration1_0_0To1_1_0;

#[async_trait]
impl<T: BlockchainSource> Migration<T> for Migration1_0_0To1_1_0 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 0,
        patch: 0,
    };

    const TO_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 1,
        patch: 0,
    };

    async fn migrate(
        &self,
        router: Arc<Router>,
        _cfg: BlockCacheConfig,
        _source: T,
    ) -> Result<(), FinalisedStateError> {
        info!("Starting v1.0.0 → v1.1.0 migration (metadata-only).");

        let mut metadata: DbMetadata = router.get_metadata().await?;

        // Advance the version marker to reflect the new API contract (v1.1.0), and refresh the
        // persisted schema hash to match the repository's recorded schema contract.
        // There are no on-disk layout changes; BlockIndex V2 is supported in-place because the
        // headers table stores a variable-length BlockHeaderData which nests a versioned BlockIndex.
        metadata.version = <Self as Migration<T>>::TO_VERSION;
        metadata.schema_hash = crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;

        // Outside of migrations this should be `Empty`. This step performs no build phases, so we
        // ensure we do not leave a stale in-progress status behind.
        metadata.migration_status = MigrationStatus::Empty;

        router.update_metadata(metadata).await?;

        info!("v1.0.0 to v1.1.0 migration complete.");
        Ok(())
    }
}
