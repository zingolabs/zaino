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
//! `Migration1_1_0To1_2_0` is a **minor in-place index backfill**, run as two sequential stages.
//!
//! Important changes in v1.2.0:
//! - The `spent` outpoint index is promoted to a core finalised-state table rather than being tied
//!   to transparent address-history support.
//! - A reverse transaction-id index (`txid_location`, `txid -> TxLocation`) is added so
//!   previous-output resolution is an O(log n) point lookup instead of a full scan of the
//!   height-keyed `txids` table.
//! - Existing databases must backfill both indices from the already-stored transparent transaction
//!   data.
//!
//! Mechanics:
//! - No shadow database is created.
//! - Stage A builds `txid_location`: it scans the raw `txids` table from genesis to the current
//!   finalised tip and writes `txid -> StoredEntryFixed<TxLocation>`. It runs first because Stage B
//!   resolves previous outputs through this index.
//! - Stage B builds `spent` + the txout-set accumulator: it reads each block's `TransparentTxList`
//!   through the existing transparent block capability and, for every non-null transparent input,
//!   writes `Outpoint -> StoredEntryFixed<TxLocation>` into the `spent` table, advancing the
//!   singleton accumulator per block.
//! - Each stage tracks its own progress as a temporary `StoredEntryFixed<Height>` entry in the
//!   metadata DB (`_migration_txid_location_progress_1_2_0_next_height` and
//!   `_migration_spent_progress_1_2_0_next_height`); both are removed on `Complete`.
//! - **0.4.0-alpha.1 compatibility (temporary):** a cache built by the alpha is recorded at v1.2.0
//!   with an empty `txid_location` index. On open, a non-empty database at version >= 1.2.0 whose
//!   `txid_location` table is empty has its `spent` table cleared and its recorded version rolled
//!   back to 1.1.0 so this migration rebuilds the indices in place. This shim is removed once 0.4.0
//!   ships.
//!
//! Safety and resumability:
//! - Deterministic: both indices are derived only from existing transparent / txid block data.
//! - Crash-resumable: each stage resumes from its own temporary progress height.
//! - Crash-safe: for each height the `spent` entries, accumulator, and progress update are committed
//!   in one LMDB transaction; `txid_location` entries and their progress likewise.
//! - Idempotent on resume: an already-present `spent` / `txid_location` entry is verified by checksum
//!   and `TxLocation`; matching entries are accepted, conflicting entries fail the migration.
//! - Re-entrant: `migrate` drives itself off the per-stage progress keys, not `migration_status`.
//! - No unsafe code and no temporary named LMDB database are used.

use super::{
    capability::{
        BlockCoreExt, Capability, DbCore as _, DbRead, DbVersion, DbWrite, MigrationStatus,
    },
    db::DbBackend,
    router::Router,
};

use crate::{
    chain_index::{
        finalised_state::{
            capability::{CapabilityRequest, DbMetadata},
            db::v1::{DB_VERSION_V1, SYNC_CHECKPOINT_INTERVAL},
            entry::{StoredEntryFixed, StoredEntryVar},
        },
        source::BlockchainSource,
        types::GENESIS_HEIGHT,
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock, Outpoint,
    TransparentTxList, TxLocation, TxidList, ZainoVersionedSerde as _,
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
        router: Arc<Router>,
        _cfg: BlockCacheConfig,
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

    async fn migrate<T: BlockchainSource>(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
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

    /// Performs the v0 → current-v1 major migration using the router’s primary/shadow model.
    ///
    /// The legacy v0 database only supports compact block data from Sapling activation onwards.
    /// The current DbV1 schema requires a complete rebuild from genesis to correctly build all indices
    /// supported by the latest v1 implementation. For this reason, this migration does not attempt
    /// partial incremental builds from Sapling; it rebuilds the current v1 schema in full in a shadow
    /// backend, then promotes it.
    ///
    /// ## Resumption behaviour
    /// If the process is shut down mid-migration:
    /// - the v1 shadow DB directory may already exist,
    /// - shadow tip height is used to resume from `shadow_tip + 1`,
    /// - and `MigrationStatus` is used as a coarse progress marker.
    ///
    /// Promotion occurs only after the current-v1 build loop has caught up to the primary tip and the
    /// shadow metadata is marked `Complete`.
    async fn migrate(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        info!("Starting v0 to v1 migration.");
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

        // The migrated v1 data is about to become primary and v0 is wiped below. Under `NO_SYNC`
        // the shadow's tail blocks and its `Complete` status may not be on disk yet; force them
        // durable now so a crash during or after promotion can never lose migrated blocks that
        // exist only in v1 (v0 is removed and cannot serve as a fallback).
        shadow.env().sync(true)?;

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

/// Flushes a buffered batch of `spent` entries (inserted in **sorted key order**) and advances the
/// Stage B progress watermark to `up_to_height + 1`, all in one LMDB transaction, then forces
/// durability.
///
/// Sorting turns the random-keyed `spent` B-tree inserts into a sequential sweep (each leaf faulted
/// in once, filled, written once) instead of a random fault per insert — the cost that dominates
/// once the DB exceeds RAM. Committing the watermark together with the entries keeps resumption
/// exact: a crash resumes from the last committed height, re-doing only uncommitted work
/// (idempotent via `NO_OVERWRITE` + verify-match). `buffer` is cleared on success.
fn flush_migration_spent_batch(
    env: &lmdb::Environment,
    spent_db: lmdb::Database,
    metadata_db: lmdb::Database,
    progress_key: &[u8],
    buffer: &mut Vec<(Vec<u8>, TxLocation)>,
    up_to_height: Height,
) -> Result<(), FinalisedStateError> {
    buffer.sort_by(|a, b| a.0.cmp(&b.0));

    let mut txn = env.begin_rw_txn()?;
    for (outpoint_bytes, tx_location) in buffer.iter() {
        let entry_bytes = StoredEntryFixed::new(outpoint_bytes, *tx_location).to_bytes()?;
        match txn.put(
            spent_db,
            outpoint_bytes,
            &entry_bytes,
            WriteFlags::NO_OVERWRITE,
        ) {
            Ok(()) => {}
            Err(lmdb::Error::KeyExist) => {
                let existing = txn
                    .get(spent_db, outpoint_bytes)
                    .map_err(FinalisedStateError::LmdbError)?;
                if existing != entry_bytes {
                    return Err(FinalisedStateError::Custom(format!(
                        "conflicting existing spent entry during batched migration for outpoint {}",
                        hex::encode(outpoint_bytes)
                    )));
                }
            }
            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
        }
    }

    let progress = StoredEntryFixed::new(progress_key, up_to_height + 1);
    txn.put(
        metadata_db,
        &progress_key,
        &progress.to_bytes()?,
        WriteFlags::empty(),
    )?;

    txn.commit()?;
    env.sync(true)?;
    buffer.clear();
    Ok(())
}

/// Minor migration: v1.1.0 → v1.2.0.
///
/// Three stages, each rebuilt deterministically from the existing transparent block data:
/// - **Stage A** — build the `txid_location` reverse index.
/// - **Stage B** — build the `spent` outpoint index.
/// - **Stage C** — build the txout-set accumulator in bulk (sequential scans) once Stage B is
///   complete, via [`DbBackend::rebuild_tx_out_set_accumulator`].
///
/// Safety and resumability:
/// - Stages A and B are resumable from per-stage progress watermarks in the metadata DB; each
///   height's index entries and its progress update commit in the same LMDB transaction.
/// - Stage C **always recomputes the accumulator from scratch** from the finalised `transparent` +
///   `spent` tables and overwrites the singleton atomically. It therefore never trusts a partial or
///   stale accumulator — including one left behind by an interrupted *original* 2-stage migration
///   that maintained the accumulator per block — so a partial run of either the old or new
///   migration converges to a correct, uncorrupted result.
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

    async fn migrate(
        &self,
        router: Arc<Router>,
        cfg: BlockCacheConfig,
        _source: T,
    ) -> Result<(), FinalisedStateError> {
        // Per-stage progress keys. Both are temporary metadata entries removed on completion.
        // Stage A (`txid_location`) and Stage B (`spent`) are tracked independently so a crash, or a
        // part-built 0.4.0-alpha.1 cache, resumes each stage from its own marker. Stage C (the
        // accumulator) needs no progress key: it is an idempotent full rebuild keyed off the tip.
        const MIGRATION_TXID_LOCATION_PROGRESS_KEY: &[u8] =
            b"_migration_txid_location_progress_1_2_0_next_height";
        const MIGRATION_SPENT_PROGRESS_KEY: &[u8] = b"_migration_spent_progress_1_2_0_next_height";

        info!("Starting v1.1.0 → v1.2.0 migration.");

        // Turn off transparent history extension while migration is in progress,
        // stopping downstream clients from recieving invalid data from ZainoDB.
        router.limit_primary_caps(Capability::TRANSPARENT_HIST_EXT);

        let backend = router.backend(CapabilityRequest::WriteCore)?;
        let env = backend.env();
        let metadata_db = backend.metadata_db()?;
        let txids_db = backend.txids_db()?;
        let transparent_db = backend.transparent_db()?;
        let spent_db = backend.spent_db()?;
        let txid_location_db = backend.txid_location_db()?;

        // Record that a migration is in progress (observability only; the migration resumes from
        // the per-stage progress keys below, not from `migration_status`).
        {
            let mut metadata: DbMetadata = router.get_metadata().await?;
            if metadata.migration_status == MigrationStatus::Empty {
                metadata.migration_status = MigrationStatus::PartialBuidInProgress;
                router.update_metadata(metadata).await?;
            }
        }

        // Reads a temporary progress height, returning `None` if the key is absent.
        let read_progress = |key: &[u8]| -> Result<Option<u32>, FinalisedStateError> {
            let txn = env.begin_ro_txn()?;
            match txn.get(metadata_db, &key) {
                Ok(bytes) => {
                    let entry = StoredEntryFixed::<Height>::from_bytes(bytes).map_err(|error| {
                        FinalisedStateError::Custom(format!(
                            "corrupt v1.2.0 migration progress entry: {error}"
                        ))
                    })?;
                    if !entry.verify(key) {
                        return Err(FinalisedStateError::Custom(
                            "v1.2.0 migration progress checksum mismatch".to_string(),
                        ));
                    }
                    Ok(Some(entry.inner().0))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(error) => Err(FinalisedStateError::LmdbError(error)),
            }
        };

        // Nothing to index or backfill on an empty database; fall through to finalisation.
        if let Some(db_tip) = router.db_height().await? {
            let db_tip = db_tip.0;

            // ===== Stage A: build the reverse txid index (`txid_location`). =====
            //
            // Stage B depends on this index to resolve previous outputs, so it is built in full
            // first. Resumes from its own progress key, so an interrupted run — or a 0.4.0-alpha.1
            // cache whose migration never built this index — continues from genesis or the last
            // committed height.
            let mut next_height =
                read_progress(MIGRATION_TXID_LOCATION_PROGRESS_KEY)?.unwrap_or(GENESIS_HEIGHT.0);

            info!(
                resume_height = next_height,
                db_tip, "v1.2.0 migration Stage A: building txid_location index"
            );
            let stage_a_started = std::time::Instant::now();

            while next_height <= db_tip {
                let height = Height::try_from(next_height)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                let height_bytes = height.to_bytes()?;

                // Read and verify the stored txid list for this height.
                let txids = {
                    let txn = env.begin_ro_txn()?;
                    let raw = txn
                        .get(txids_db, &height_bytes)
                        .map_err(FinalisedStateError::LmdbError)?;
                    let entry = StoredEntryVar::<TxidList>::from_bytes(raw).map_err(|error| {
                        FinalisedStateError::Custom(format!("txids corrupt data: {error}"))
                    })?;
                    if !entry.verify(&height_bytes) {
                        return Err(FinalisedStateError::Custom(
                            "txids checksum mismatch".to_string(),
                        ));
                    }
                    entry.inner().txids().to_vec()
                };

                // Reverse-index entries, sorted by txid so the random-keyed B-tree inserts locally.
                let mut entries: Vec<([u8; 32], TxLocation)> = Vec::with_capacity(txids.len());
                for (tx_index, txid) in txids.iter().enumerate() {
                    let tx_index = u16::try_from(tx_index).map_err(|_| {
                        FinalisedStateError::Custom(format!(
                            "transaction index out of range at height {}",
                            height.0
                        ))
                    })?;
                    entries.push(((*txid).into(), TxLocation::new(height.0, tx_index)));
                }
                entries.sort_by_key(|entry| entry.0);

                // Write the height's entries and advance Stage A progress atomically.
                {
                    let mut txn = env.begin_rw_txn()?;

                    for (txid_bytes, tx_location) in &entries {
                        let entry_bytes =
                            StoredEntryFixed::new(txid_bytes, *tx_location).to_bytes()?;

                        match txn.put(
                            txid_location_db,
                            txid_bytes,
                            &entry_bytes,
                            WriteFlags::NO_OVERWRITE,
                        ) {
                            Ok(()) => {}

                            // Idempotent on resume: an existing entry must match exactly.
                            Err(lmdb::Error::KeyExist) => {
                                let existing_bytes = txn
                                    .get(txid_location_db, txid_bytes)
                                    .map_err(FinalisedStateError::LmdbError)?;
                                let existing_entry =
                                    StoredEntryFixed::<TxLocation>::from_bytes(existing_bytes)
                                        .map_err(|error| {
                                            FinalisedStateError::Custom(format!(
                                                "corrupt existing txid_location entry: {error}"
                                            ))
                                        })?;
                                if !existing_entry.verify(txid_bytes) {
                                    return Err(FinalisedStateError::Custom(
                                        "existing txid_location entry checksum mismatch"
                                            .to_string(),
                                    ));
                                }
                                if existing_entry.inner() != tx_location {
                                    return Err(FinalisedStateError::Custom(format!(
                                        "conflicting txid_location entry at height {}",
                                        height.0
                                    )));
                                }
                            }

                            Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                        }
                    }

                    let progress =
                        StoredEntryFixed::new(MIGRATION_TXID_LOCATION_PROGRESS_KEY, height + 1);
                    txn.put(
                        metadata_db,
                        &MIGRATION_TXID_LOCATION_PROGRESS_KEY,
                        &progress.to_bytes()?,
                        WriteFlags::empty(),
                    )?;

                    txn.commit()?;
                }

                // Durability checkpoint (the env is opened with `NO_SYNC`): bound how much
                // backfill a crash can discard. The lost tail is re-done idempotently from the
                // Stage A progress key on resume.
                if next_height % SYNC_CHECKPOINT_INTERVAL == 0 {
                    env.sync(true)?;
                }

                if next_height % 50_000 == 0 {
                    info!(
                        height = next_height,
                        db_tip,
                        elapsed = ?stage_a_started.elapsed(),
                        "v1.2.0 migration Stage A progress"
                    );
                }

                next_height = height.0 + 1;
            }

            // Make the completed `txid_location` index a durable boundary so a crash during
            // Stage B never has to re-run Stage A.
            env.sync(true)?;

            info!(
                db_tip,
                elapsed = ?stage_a_started.elapsed(),
                "v1.2.0 migration Stage A complete"
            );

            // ===== Stage B: backfill the `spent` outpoint index. =====
            //
            // Resumes from its own progress key, preserving partial work from an interrupted
            // migration. If the key is absent (fresh, or a completed alpha cache rolled back to
            // v1.1.0) it starts at genesis. The accumulator is intentionally *not* touched here — it
            // is built in full by Stage C below, so an interrupted original 2-stage migration that
            // left a partial per-block accumulator is simply overwritten, never trusted.
            let mut next_height_to_migrate = match read_progress(MIGRATION_SPENT_PROGRESS_KEY)? {
                Some(height) => height,
                None => {
                    let mut txn = env.begin_rw_txn()?;

                    let progress =
                        StoredEntryFixed::new(MIGRATION_SPENT_PROGRESS_KEY, GENESIS_HEIGHT);
                    txn.put(
                        metadata_db,
                        &MIGRATION_SPENT_PROGRESS_KEY,
                        &progress.to_bytes()?,
                        WriteFlags::empty(),
                    )?;

                    txn.commit()?;
                    GENESIS_HEIGHT.0
                }
            };

            // Re-read the tip in case the chain advanced while Stage A was running.
            let db_tip = router
                .db_height()
                .await?
                .map(|height| height.0)
                .unwrap_or(db_tip);

            info!(
                resume_height = next_height_to_migrate,
                db_tip, "v1.2.0 migration Stage B: backfilling spent index"
            );
            let stage_b_started = std::time::Instant::now();

            // Buffer spent entries across heights, then flush them in sorted key order so the
            // random-keyed `spent` B-tree fills via a sequential sweep instead of a random fault per
            // insert. Each flush commits the entries together with the progress watermark.
            let batch_budget = cfg.storage.database.sync_write_batch_bytes.max(1);
            let mut spent_buffer: Vec<(Vec<u8>, TxLocation)> = Vec::new();
            let mut spent_buffer_bytes: u64 = 0;

            while next_height_to_migrate <= db_tip {
                let height = Height::try_from(next_height_to_migrate)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                let height_bytes = height.to_bytes()?;

                // Read the stored transparent list directly from the table. This intentionally
                // bypasses the `BlockTransparentExt` accessor, which routes through
                // `resolve_validated_hash_or_height` → `validate_block_blocking` (merkle-root
                // recompute + full-payload checksum verification) for every height above
                // `validated_tip`. During migration `validated_tip` is still climbing on the
                // background validator, so that path would re-validate the whole chain inside the
                // backfill loop — pure redundant CPU. The data here is already on disk and trusted;
                // Stage A reads `txids` the same raw way.
                let transparent_tx_list = {
                    let txn = env.begin_ro_txn()?;
                    let raw = txn
                        .get(transparent_db, &height_bytes)
                        .map_err(FinalisedStateError::LmdbError)?;
                    let entry =
                        StoredEntryVar::<TransparentTxList>::from_bytes(raw).map_err(|error| {
                            FinalisedStateError::Custom(format!(
                                "transparent corrupt data: {error}"
                            ))
                        })?;
                    if !entry.verify(&height_bytes) {
                        return Err(FinalisedStateError::Custom(
                            "transparent checksum mismatch".to_string(),
                        ));
                    }
                    entry.inner().clone()
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

                        let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());

                        if spent_map.insert(outpoint, tx_location).is_some() {
                            return Err(FinalisedStateError::Custom(format!(
                                "duplicate transparent spend for outpoint {:?} at height {}",
                                outpoint, height.0
                            )));
                        }
                    }
                }

                // Append this height's spent entries to the batch buffer. The flush (below) sorts
                // them by key and commits them with the progress watermark in one transaction.
                for (outpoint, tx_location) in &spent_map {
                    let outpoint_bytes = outpoint.to_bytes()?;
                    spent_buffer_bytes =
                        spent_buffer_bytes.saturating_add(outpoint_bytes.len() as u64 + 64);
                    spent_buffer.push((outpoint_bytes, *tx_location));
                }

                // Flush a full batch: sorted `spent` insert + progress watermark = `height + 1`,
                // committed atomically and fsynced (env is `NO_SYNC`). A crash resumes from the last
                // committed height; re-done work is idempotent (`NO_OVERWRITE` + verify-match).
                if spent_buffer_bytes >= batch_budget {
                    flush_migration_spent_batch(
                        &env,
                        spent_db,
                        metadata_db,
                        MIGRATION_SPENT_PROGRESS_KEY,
                        &mut spent_buffer,
                        height,
                    )?;
                    spent_buffer_bytes = 0;
                }

                if next_height_to_migrate % 10_000 == 0 {
                    info!(
                        height = next_height_to_migrate,
                        db_tip,
                        elapsed = ?stage_b_started.elapsed(),
                        "v1.2.0 migration Stage B progress"
                    );
                }

                next_height_to_migrate = height.0 + 1;
            }

            // Flush the trailing partial batch (progress watermark = db tip).
            if !spent_buffer.is_empty() {
                let tip_height = Height::try_from(db_tip)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                flush_migration_spent_batch(
                    &env,
                    spent_db,
                    metadata_db,
                    MIGRATION_SPENT_PROGRESS_KEY,
                    &mut spent_buffer,
                    tip_height,
                )?;
            }

            info!(
                db_tip,
                elapsed = ?stage_b_started.elapsed(),
                "v1.2.0 migration Stage B complete"
            );

            // ===== Stage C: build the txout-set accumulator in bulk. =====
            //
            // Recomputes the accumulator from scratch over the finalised `transparent` + `spent`
            // tables (built by Stage B) and overwrites the singleton atomically. This is the step
            // that makes the migration robust to partial prior runs: it never reads or trusts an
            // existing accumulator, so a stale per-block accumulator from an interrupted original
            // migration is discarded and replaced with a correct value. It is idempotent, so a crash
            // mid-Stage-C is recovered by simply re-running the (skipped) earlier stages and
            // rebuilding again.
            let stage_c_started = std::time::Instant::now();
            info!(
                db_tip,
                "v1.2.0 migration Stage C: building txout-set accumulator"
            );
            backend.rebuild_tx_out_set_accumulator().await?;
            info!(
                db_tip,
                elapsed = ?stage_c_started.elapsed(),
                "v1.2.0 migration Stage C complete"
            );
        }

        // ===== Finalise: advance metadata to v1.2.0, then remove the progress keys. =====
        //
        // Ordering matters under `NO_SYNC`. The recorded version is the migration's completion
        // gate, so it must become durable *before* the progress keys are removed:
        //
        // 1. Flush all backfilled `spent` / accumulator work so the version we are about to
        //    record truthfully reflects on-disk state.
        // 2. Record version v1.2.0 and force it durable. A crash before this leaves the version
        //    < v1.2.0 with the progress keys intact, so the migration is re-selected and resumes
        //    cheaply (the stages skip past `db_tip`, then re-finalise).
        // 3. Only now remove the progress keys: the version gate is durably set, so they are
        //    dead metadata. Removing them last guarantees a crash never leaves "keys deleted but
        //    version still v1.1.0", which would force a full, wasteful re-migration.
        env.sync(true)?;

        let mut metadata: DbMetadata = router.get_metadata().await?;
        metadata.version = <Self as Migration<T>>::TO_VERSION;
        metadata.schema_hash = crate::chain_index::finalised_state::db::v1::DB_SCHEMA_V1_HASH;
        metadata.migration_status = MigrationStatus::Empty;
        router.update_metadata(metadata).await?;
        env.sync(true)?;

        {
            let mut txn = env.begin_rw_txn()?;

            for key in [
                MIGRATION_TXID_LOCATION_PROGRESS_KEY,
                MIGRATION_SPENT_PROGRESS_KEY,
            ] {
                match txn.del(metadata_db, &key, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                }
            }

            txn.commit()?;
        }
        env.sync(true)?;

        // Turn transparent history extension back on now the indices are built.
        router.extend_primary_caps(Capability::TRANSPARENT_HIST_EXT);

        info!("v1.1.0 to v1.2.0 migration complete.");
        Ok(())
    }
}
