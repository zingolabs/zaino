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
//! ## v0 → v1
//!
//! `Migration0To1` performs a **full shadow rebuild from genesis**.
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
//!
//! ## v1.1.0 → v1.2.0
//!
//! `Migration1_1_0To1_2_0` is a **minor in-place index backfill**.
//!
//! Important changes in v1.2.0:
//! - The `spent` outpoint index is promoted to a core finalised-state table rather than being tied
//!   to transparent address-history support.
//! - Existing databases must backfill `spent` from the already-stored transparent transaction data.
//!
//! Mechanics:
//! - No shadow database is created.
//! - The migration reads each block’s `TransparentTxList` through the existing transparent block
//!   capability.
//! - For every non-null transparent input, it writes:
//!   `Outpoint -> StoredEntryFixed<TxLocation>`
//!   into the `spent` table.
//! - Progress is stored as a temporary `StoredEntryFixed<Height>` entry in the existing metadata DB
//!   under `_migration_spent_progress_1_2_0_next_height`.
//! - The temporary progress entry is removed once the migration reaches `Complete`.
//!
//! Safety and resumability:
//! - Deterministic: the `spent` index is derived only from existing transparent block data.
//! - Crash-resumable: the temporary progress height records the next block height to migrate.
//! - Crash-safe: spent entries for a height and the progress update are committed in the same LMDB
//!   write transaction.
//! - Idempotent on resume: if a spent entry already exists, the migration verifies its checksum and
//!   `TxLocation`; matching entries are accepted, conflicting entries fail the migration.
//! - No unsafe code and no temporary named LMDB database are used.

use super::{
    capability::{BlockCoreExt, DbCore as _, DbRead, DbVersion, DbWrite, MigrationStatus},
    db::DbBackend,
    router::Router,
};

use crate::{
    chain_index::{
        finalised_state::{
            capability::{BlockTransparentExt as _, DbMetadata},
            db::v1::{DB_VERSION_V1, TX_OUT_SET_INFO_ACCUMULATOR_KEY},
            entry::StoredEntryFixed,
            router::StatelessMode,
        },
        source::BlockchainSource,
        types::{db::metadata::FinalisedTxOutSetInfoAccumulator, GENESIS_HEIGHT},
    },
    config::ChainIndexConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock, Outpoint,
    TransactionHash, TransparentCompactTx, TxLocation, ZainoVersionedSerde as _,
};

use lmdb::{Transaction, WriteFlags};
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

    /// Returns the routing/lifecycle category for this migration.
    ///
    /// Patch migrations run directly against the current routed primary state and use the default
    /// metadata-only migration implementation.
    ///
    /// Minor and major migrations are run while the migration manager holds a full-mode stateless
    /// reference. During that time normal service capabilities route to stateless and migration code
    /// must use direct maintenance access to the persistent backend or replacement backend.
    fn migration_type(&self) -> MigrationType {
        MigrationType::Patch
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
    ///
    /// **Default**: Metadata-only migration.
    ///
    /// Use this for migrations where no LMDB data layout changes are required.
    async fn migrate(
        &self,
        router: Arc<Router<T>>,
        _cfg: ChainIndexConfig,
        _source: T,
    ) -> Result<(), FinalisedStateError> {
        info!(
            "Starting metadata-only migration from {} to {}.",
            Self::CURRENT_VERSION,
            Self::TO_VERSION,
        );

        let mut metadata: DbMetadata = router.get_metadata().await?;

        metadata.version = Self::TO_VERSION;
        metadata.schema_hash = crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;
        metadata.migration_status = MigrationStatus::Empty;

        router.update_metadata(metadata).await?;

        info!(
            "Metadata-only migration from {} to {} complete.",
            Self::CURRENT_VERSION,
            Self::TO_VERSION,
        );

        Ok(())
    }
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
    pub(super) router: Arc<Router<T>>,

    /// Block-cache configuration (paths, network, configured target DB version, etc.).
    pub(super) cfg: ChainIndexConfig,

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
            let migration_type = migration.migration_type::<T>();

            match migration_type {
                MigrationType::Patch => {
                    migration
                        .migrate(
                            Arc::clone(&self.router),
                            self.cfg.clone(),
                            self.source.clone(),
                        )
                        .await?;
                }

                MigrationType::Minor | MigrationType::Major => {
                    let primary = self.router.primary_backend();
                    let db_height = primary.db_height().await?;

                    let _stateless_reference = self
                        .router
                        .init_or_take_stateless(
                            self.source.clone(),
                            self.cfg.network.to_zebra_network(),
                            StatelessMode::Full,
                            db_height,
                        )
                        .await?;

                    migration
                        .migrate(
                            Arc::clone(&self.router),
                            self.cfg.clone(),
                            self.source.clone(),
                        )
                        .await?;
                }
            }

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
            (0, 0, 0) => Ok(MigrationStep::Migration0To1(Migration0To1)),
            (1, 0, 0) => Ok(MigrationStep::Migration1_0_0To1_1_0(Migration1_0_0To1_1_0)),
            (1, 1, 0) => Ok(MigrationStep::Migration1_1_0To1_2_0(Migration1_1_0To1_2_0)),
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
    Migration0To1(Migration0To1),
    Migration1_0_0To1_1_0(Migration1_0_0To1_1_0),
    Migration1_1_0To1_2_0(Migration1_1_0To1_2_0),
}

impl MigrationStep {
    fn to_version<T: BlockchainSource>(&self) -> DbVersion {
        match self {
            MigrationStep::Migration0To1(_step) => <Migration0To1 as Migration<T>>::TO_VERSION,
            MigrationStep::Migration1_0_0To1_1_0(_step) => {
                <Migration1_0_0To1_1_0 as Migration<T>>::TO_VERSION
            }
            MigrationStep::Migration1_1_0To1_2_0(_step) => {
                <Migration1_1_0To1_2_0 as Migration<T>>::TO_VERSION
            }
        }
    }

    fn migration_type<T: BlockchainSource>(&self) -> MigrationType {
        match self {
            MigrationStep::Migration0To1(step) => {
                <Migration0To1 as Migration<T>>::migration_type(step)
            }
            MigrationStep::Migration1_0_0To1_1_0(step) => {
                <Migration1_0_0To1_1_0 as Migration<T>>::migration_type(step)
            }
            MigrationStep::Migration1_1_0To1_2_0(step) => {
                <Migration1_1_0To1_2_0 as Migration<T>>::migration_type(step)
            }
        }
    }

    async fn migrate<T: BlockchainSource>(
        &self,
        router: Arc<Router<T>>,
        cfg: ChainIndexConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        match self {
            MigrationStep::Migration0To1(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_0_0To1_1_0(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_1_0To1_2_0(step) => step.migrate(router, cfg, source).await,
        }
    }
}

// ***** Migrations *****

/// Major migration: v0.0.0 → current v1.
///
/// This migration performs a shadow rebuild of the **current** v1 database from genesis, then
/// promotes the completed shadow to primary and schedules deletion of the old v0 database directory
/// once all handles are dropped.
///
/// This was previously documented as `v0.0.0 → v1.0.0`, but that was incorrect: the shadow backend
/// is created with `DbBackend::spawn_v1`, which opens or creates the latest supported v1 schema
/// identified by `DB_VERSION_V1`.
///
/// See the module-level documentation for the detailed rationale and mechanics.
struct Migration0To1;

#[async_trait]
impl<T: BlockchainSource> Migration<T> for Migration0To1 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 0,
        minor: 0,
        patch: 0,
    };
    const TO_VERSION: DbVersion = DB_VERSION_V1;

    fn migration_type(&self) -> MigrationType {
        MigrationType::Major
    }

    async fn migrate(
        &self,
        router: Arc<Router<T>>,
        cfg: ChainIndexConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        info!("Starting v0 to v1 migration.");

        let old_primary = router.primary_backend();
        let replacement = Arc::new(DbBackend::spawn_v1(&cfg).await?);

        let migration_status = replacement.get_metadata().await?.migration_status();

        match migration_status {
            MigrationStatus::Empty
            | MigrationStatus::PartialBuidInProgress
            | MigrationStatus::PartialBuildComplete
            | MigrationStatus::FinalBuildInProgress => {
                let mut parent_chain_work = ChainWork::from_u256(0.into());

                let replacement_db_height_opt = replacement.db_height().await?;
                let replacement_db_height = replacement_db_height_opt.unwrap_or(GENESIS_HEIGHT);

                let build_start_height = if replacement_db_height_opt.is_some() {
                    parent_chain_work = replacement
                        .get_block_header(replacement_db_height)
                        .await?
                        .context
                        .chainwork;

                    replacement_db_height + 1
                } else {
                    replacement_db_height
                };

                let primary_db_height = old_primary.db_height().await?.unwrap_or(GENESIS_HEIGHT);

                info!(
                "Starting replacement database build, current database tips: old primary:{} replacement:{}",
                primary_db_height, replacement_db_height
                );

                if replacement_db_height < primary_db_height {
                    for height in build_start_height.0..=primary_db_height.0 {
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

                        let chain_block_height = chain_block.height();

                        parent_chain_work = *chain_block.chainwork();

                        replacement.write_block(chain_block).await?;

                        router.update_stateless_db_height(Some(chain_block_height))?;
                    }
                }

                let mut metadata = replacement.get_metadata().await?;
                metadata.migration_status = MigrationStatus::Complete;
                replacement.update_metadata(metadata).await?;

                info!("v1 replacement database build complete.");
            }

            MigrationStatus::Complete => {
                info!("v1 replacement database was already marked complete.");
            }
        }

        info!("Replacing primary with rebuilt v1 database.");

        let old_primary = router.replace_primary(Arc::clone(&replacement));

        router.update_stateless_db_height(replacement.db_height().await?)?;

        tokio::spawn(async move {
            while Arc::strong_count(&old_primary) > 1 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            if let Err(error) = old_primary.shutdown().await {
                tracing::warn!("Old primary shutdown failed: {error}");
            }

            let db_path_dir = match cfg.network.to_zebra_network().kind() {
                NetworkKind::Mainnet => "live",
                NetworkKind::Testnet => "test",
                NetworkKind::Regtest => "local",
            };

            let db_path = cfg.storage.database.path.join(db_path_dir);

            info!("Wiping v0 database from disk.");

            match tokio::fs::remove_dir_all(&db_path).await {
                Ok(()) => {
                    tracing::info!("Deleted old database at {}", db_path.display());
                }
                Err(error) => {
                    tracing::error!(
                        "Failed to delete old database at {}: {}",
                        db_path.display(),
                        error
                    );
                }
            }
        });

        info!("v0 to v1 migration complete.");

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
}

/// Minor migration: v1.1.0 → v1.2.0.
///
/// Safety and resumability:
/// - Deterministic: rebuilds the spent outpoint index and txout-set accumulator from the existing
///   transparent block data.
/// - Resumable: stores the next height to migrate in the metadata DB under a temporary migration key.
/// - Crash-safe: each block's spent entries, txout-set accumulator, and progress update are
///   committed in the same LMDB transaction.
/// - No shadow database.
struct Migration1_1_0To1_2_0;

#[async_trait]
impl<T: BlockchainSource> Migration<T> for Migration1_1_0To1_2_0 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 1,
        patch: 0,
    };

    const TO_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 2,
        patch: 0,
    };

    fn migration_type(&self) -> MigrationType {
        MigrationType::Minor
    }

    async fn migrate(
        &self,
        router: Arc<Router<T>>,
        _cfg: ChainIndexConfig,
        _source: T,
    ) -> Result<(), FinalisedStateError> {
        const MIGRATION_SPENT_PROGRESS_KEY: &[u8] = b"_migration_spent_progress_1_2_0_next_height";

        info!("Starting v1.1.0 → v1.2.0 migration.");

        let backend = router.primary_backend();

        let env = backend.env()?;
        let metadata_db = backend.metadata_db()?;
        let spent_db = backend.spent_db()?;
        let tx_out_set_info_accumulator_db = backend.tx_out_set_info_accumulator_db()?;

        loop {
            match backend.get_metadata().await?.migration_status() {
                MigrationStatus::Empty => {
                    let mut metadata: DbMetadata = backend.get_metadata().await?;
                    metadata.migration_status = MigrationStatus::PartialBuidInProgress;

                    {
                        let mut txn = env.begin_rw_txn()?;

                        let next_height_entry =
                            StoredEntryFixed::new(MIGRATION_SPENT_PROGRESS_KEY, GENESIS_HEIGHT);
                        let next_height_bytes = next_height_entry.to_bytes()?;

                        txn.put(
                            metadata_db,
                            &MIGRATION_SPENT_PROGRESS_KEY,
                            &next_height_bytes,
                            WriteFlags::empty(),
                        )?;

                        let tx_out_set_info_accumulator_entry = StoredEntryFixed::new(
                            TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                            FinalisedTxOutSetInfoAccumulator::empty(),
                        );

                        txn.put(
                            tx_out_set_info_accumulator_db,
                            &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                            &tx_out_set_info_accumulator_entry.to_bytes()?,
                            WriteFlags::empty(),
                        )?;

                        let metadata_key = b"metadata";
                        let metadata_entry_bytes =
                            StoredEntryFixed::new(metadata_key, metadata).to_bytes()?;

                        txn.put(
                            metadata_db,
                            metadata_key,
                            &metadata_entry_bytes,
                            WriteFlags::empty(),
                        )?;

                        txn.commit()?;
                    }
                }

                MigrationStatus::PartialBuidInProgress
                | MigrationStatus::PartialBuildComplete
                | MigrationStatus::FinalBuildInProgress => {
                    let mut next_height_to_migrate = {
                        let txn = env.begin_ro_txn()?;

                        let height_bytes = match txn.get(metadata_db, &MIGRATION_SPENT_PROGRESS_KEY)
                        {
                            Ok(height_bytes) => height_bytes,
                            Err(lmdb::Error::NotFound) => {
                                return Err(FinalisedStateError::Custom(
                                    "missing v1.2.0 spent migration progress key".to_string(),
                                ));
                            }
                            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                        };

                        let height_entry = StoredEntryFixed::<Height>::from_bytes(height_bytes)
                            .map_err(|error| {
                                FinalisedStateError::Custom(format!(
                                    "corrupt v1.2.0 spent migration progress entry: {error}"
                                ))
                            })?;

                        if !height_entry.verify(MIGRATION_SPENT_PROGRESS_KEY) {
                            return Err(FinalisedStateError::Custom(
                                "v1.2.0 spent migration progress checksum mismatch".to_string(),
                            ));
                        }

                        height_entry.inner().0
                    };

                    let Some(db_height) = backend.db_height().await? else {
                        let mut metadata: DbMetadata = backend.get_metadata().await?;
                        metadata.migration_status = MigrationStatus::Complete;
                        backend.update_metadata(metadata).await?;
                        continue;
                    };

                    router.update_stateless_db_height(Some(db_height))?;

                    let db_height = db_height.0;

                    while next_height_to_migrate <= db_height {
                        let height = Height::try_from(next_height_to_migrate)
                            .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;

                        let transparent_tx_list = backend.get_block_transparent(height).await?;

                        let txids = {
                            let mut txids = Vec::with_capacity(transparent_tx_list.tx().len());

                            for tx_index in 0..transparent_tx_list.tx().len() {
                                let tx_index = u16::try_from(tx_index).map_err(|_| {
                                    FinalisedStateError::Custom(format!(
                                        "transaction index out of range at height {}",
                                        height.0
                                    ))
                                })?;

                                let tx_location = TxLocation::new(height.0, tx_index);
                                let txid = backend.get_txid(tx_location).await?;

                                txids.push(txid);
                            }

                            txids
                        };

                        let transparent = transparent_tx_list.tx().to_vec();

                        let mut spent_map = std::collections::HashMap::new();

                        for (tx_index, tx_opt) in transparent.iter().enumerate() {
                            let Some(transparent_tx) = tx_opt else {
                                continue;
                            };

                            let tx_index = u16::try_from(tx_index).map_err(|_| {
                                FinalisedStateError::Custom(format!(
                                    "transaction index out of range at height {}",
                                    height.0
                                ))
                            })?;

                            let tx_location = TxLocation::new(height.0, tx_index);

                            for input in transparent_tx.inputs() {
                                if input.is_null_prevout() {
                                    continue;
                                }

                                let outpoint =
                                    Outpoint::new(*input.prevout_txid(), input.prevout_index());

                                if spent_map.insert(outpoint, tx_location).is_some() {
                                    return Err(FinalisedStateError::Custom(format!(
                                    "duplicate transparent spend for outpoint {:?} at height {}",
                                    outpoint, height.0
                                )));
                                }
                            }
                        }

                        let tx_out_set_info_accumulator = match backend.as_ref() {
                            DbBackend::V1(database) => {
                                let transactions: Vec<(
                                    TransactionHash,
                                    Option<TransparentCompactTx>,
                                )> = txids
                                    .iter()
                                    .copied()
                                    .zip(transparent.iter().cloned())
                                    .collect();

                                database
                                    .calculate_tx_out_set_info_accumulator_after_block(
                                        height,
                                        &transactions,
                                        &spent_map,
                                    )
                                    .await?
                            }
                            DbBackend::V0(_) | DbBackend::Stateless(_) => {
                                return Err(FinalisedStateError::FeatureUnavailable(
                                    "v1 txout-set accumulator migration",
                                ));
                            }
                        };

                        {
                            let mut txn = env.begin_rw_txn()?;

                            for (outpoint, tx_location) in &spent_map {
                                let outpoint_bytes = outpoint.to_bytes()?;
                                let tx_location_entry_bytes =
                                    StoredEntryFixed::new(&outpoint_bytes, *tx_location)
                                        .to_bytes()?;

                                match txn.put(
                                    spent_db,
                                    &outpoint_bytes,
                                    &tx_location_entry_bytes,
                                    WriteFlags::NO_OVERWRITE,
                                ) {
                                    Ok(()) => {}

                                    Err(lmdb::Error::KeyExist) => {
                                        let existing_bytes = txn
                                            .get(spent_db, &outpoint_bytes)
                                            .map_err(FinalisedStateError::LmdbError)?;

                                        let existing_entry =
                                        StoredEntryFixed::<TxLocation>::from_bytes(existing_bytes)
                                            .map_err(|error| {
                                                FinalisedStateError::Custom(format!(
                                                    "corrupt existing spent entry for outpoint {:?}: {error}",
                                                    outpoint
                                                ))
                                            })?;

                                        if !existing_entry.verify(&outpoint_bytes) {
                                            return Err(FinalisedStateError::Custom(format!(
                                            "existing spent entry checksum mismatch for outpoint {:?}",
                                            outpoint
                                        )));
                                        }

                                        if existing_entry.inner() != tx_location {
                                            return Err(FinalisedStateError::Custom(format!(
                                            "conflicting spent entry for outpoint {:?} at height {}",
                                            outpoint, height.0
                                        )));
                                        }
                                    }

                                    Err(error) => {
                                        return Err(FinalisedStateError::LmdbError(error));
                                    }
                                }
                            }

                            let tx_out_set_info_accumulator_entry = StoredEntryFixed::new(
                                TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                                tx_out_set_info_accumulator,
                            );

                            txn.put(
                                tx_out_set_info_accumulator_db,
                                &TX_OUT_SET_INFO_ACCUMULATOR_KEY,
                                &tx_out_set_info_accumulator_entry.to_bytes()?,
                                WriteFlags::empty(),
                            )?;

                            let next_height = height + 1;

                            let next_height_entry =
                                StoredEntryFixed::new(MIGRATION_SPENT_PROGRESS_KEY, next_height);
                            let next_height_bytes = next_height_entry.to_bytes()?;

                            txn.put(
                                metadata_db,
                                &MIGRATION_SPENT_PROGRESS_KEY,
                                &next_height_bytes,
                                WriteFlags::empty(),
                            )?;

                            txn.commit()?;
                        }

                        router.update_stateless_db_height(Some(Height(next_height_to_migrate)))?;

                        next_height_to_migrate = height.0 + 1;
                    }

                    let mut metadata: DbMetadata = backend.get_metadata().await?;
                    metadata.migration_status = MigrationStatus::Complete;
                    backend.update_metadata(metadata).await?;
                }

                MigrationStatus::Complete => {
                    {
                        let mut txn = env.begin_rw_txn()?;

                        match txn.del(metadata_db, &MIGRATION_SPENT_PROGRESS_KEY, None) {
                            Ok(()) | Err(lmdb::Error::NotFound) => {}
                            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                        }

                        txn.commit()?;
                    }

                    let mut metadata: DbMetadata = backend.get_metadata().await?;

                    metadata.version = <Self as Migration<T>>::TO_VERSION;
                    metadata.schema_hash =
                        crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;
                    metadata.migration_status = MigrationStatus::Empty;

                    backend.update_metadata(metadata).await?;

                    break;
                }
            }
        }

        info!("v1.1.0 to v1.2.0 migration complete.");

        Ok(())
    }
}
