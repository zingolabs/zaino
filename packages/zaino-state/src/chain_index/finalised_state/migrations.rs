//! Database version migration framework and implementations
//!
//! This file defines how `FinalisedState` migrates on-disk databases between database versions.
//!
//! Migrations are orchestrated by [`MigrationManager`], which is invoked from `FinalisedState::spawn` when
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
//! ## v1.0.0 → v1.1.0
//!
//! `Migration1_0_0To1_1_0` is a **minor version bump** with **on disk schema changes**, but does
//! not include changes to the external FinalisedState API.
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
    capability::{DbRead, DbVersion, DbWrite, MigrationStatus},
    router::Router,
};

use crate::{
    chain_index::{
        finalised_state::{
            capability::DbMetadata,
            entry::{StoredEntryFixed, StoredEntryVar},
            finalised_source::v1::SYNC_CHECKPOINT_INTERVAL,
            router::EphemeralMode,
        },
        source::BlockchainSource,
        types::GENESIS_HEIGHT,
    },
    config::ChainIndexConfig,
    error::FinalisedStateError,
    CommitmentTreeData, Height, TransparentTxList, TxLocation, TxidList, ZainoVersionedSerde as _,
};

use lmdb::{Transaction, WriteFlags};

use crate::SendFut;

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
    /// Minor and major migrations are run while the migration manager holds a full-mode ephemeral
    /// reference. During that time normal service capabilities route to ephemeral and migration code
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
    fn migrate(
        &self,
        router: Arc<Router<T>>,
        _cfg: ChainIndexConfig,
        _source: T,
    ) -> impl SendFut<Result<(), FinalisedStateError>> {
        async move {
            info!(
                from = %Self::CURRENT_VERSION,
                to = %Self::TO_VERSION,
                "starting metadata-only migration"
            );

            let mut metadata: DbMetadata = router.get_metadata().await?;

            metadata.version = Self::TO_VERSION;
            metadata.schema_hash =
                crate::chain_index::finalised_state::finalised_source::v1::DB_SCHEMA_V1_HASH;
            metadata.migration_status = MigrationStatus::Empty;

            router.update_metadata(metadata).await?;

            info!(
                from = %Self::CURRENT_VERSION,
                to = %Self::TO_VERSION,
                "metadata-only migration complete"
            );

            Ok(())
        }
    }
}

/// Orchestrates a sequence of migration steps until `target_version` is reached.
///
/// `MigrationManager` is constructed by `FinalisedState::spawn` when it detects that the on-disk database
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

                    let _ephemeral_reference = self
                        .router
                        .init_or_take_ephemeral(
                            self.source.clone(),
                            self.cfg.network.clone(),
                            EphemeralMode::Full,
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
            (1, 0, 0) => Ok(MigrationStep::Migration1_0_0To1_1_0(Migration1_0_0To1_1_0)),
            (1, 1, 0) => Ok(MigrationStep::Migration1_1_0To1_2_0(Migration1_1_0To1_2_0)),
            (1, 2, 0) => Ok(MigrationStep::Migration1_2_0To1_2_1(Migration1_2_0To1_2_1)),
            (1, 2, 1) => Ok(MigrationStep::Migration1_2_1To1_3_0(Migration1_2_1To1_3_0)),
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
    Migration1_0_0To1_1_0(Migration1_0_0To1_1_0),
    Migration1_1_0To1_2_0(Migration1_1_0To1_2_0),
    Migration1_2_0To1_2_1(Migration1_2_0To1_2_1),
    Migration1_2_1To1_3_0(Migration1_2_1To1_3_0),
}

impl MigrationStep {
    fn to_version<T: BlockchainSource>(&self) -> DbVersion {
        match self {
            MigrationStep::Migration1_0_0To1_1_0(_step) => {
                <Migration1_0_0To1_1_0 as Migration<T>>::TO_VERSION
            }
            MigrationStep::Migration1_1_0To1_2_0(_step) => {
                <Migration1_1_0To1_2_0 as Migration<T>>::TO_VERSION
            }
            MigrationStep::Migration1_2_0To1_2_1(_step) => {
                <Migration1_2_0To1_2_1 as Migration<T>>::TO_VERSION
            }
            MigrationStep::Migration1_2_1To1_3_0(_step) => {
                <Migration1_2_1To1_3_0 as Migration<T>>::TO_VERSION
            }
        }
    }

    fn migration_type<T: BlockchainSource>(&self) -> MigrationType {
        match self {
            MigrationStep::Migration1_0_0To1_1_0(step) => {
                <Migration1_0_0To1_1_0 as Migration<T>>::migration_type(step)
            }
            MigrationStep::Migration1_1_0To1_2_0(step) => {
                <Migration1_1_0To1_2_0 as Migration<T>>::migration_type(step)
            }
            MigrationStep::Migration1_2_0To1_2_1(step) => {
                <Migration1_2_0To1_2_1 as Migration<T>>::migration_type(step)
            }
            MigrationStep::Migration1_2_1To1_3_0(step) => {
                <Migration1_2_1To1_3_0 as Migration<T>>::migration_type(step)
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
            MigrationStep::Migration1_0_0To1_1_0(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_1_0To1_2_0(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_2_0To1_2_1(step) => step.migrate(router, cfg, source).await,
            MigrationStep::Migration1_2_1To1_3_0(step) => step.migrate(router, cfg, source).await,
        }
    }
}

// ***** Migrations *****

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

/// Writes `value` under `key` with `NO_OVERWRITE`, tolerating an existing byte-identical row.
///
/// Migrations use this to stay idempotent on crash-resume: a resumed pass may revisit rows it has
/// already committed, and each such row must match the rebuilt bytes exactly. Any difference —
/// a conflicting rebuild or a corrupt existing row — aborts the migration with the message
/// `describe` produces. `describe` runs only on that error path, so the per-row success path
/// allocates nothing.
fn put_idempotent(
    txn: &mut lmdb::RwTransaction<'_>,
    db: lmdb::Database,
    key: &[u8],
    value: &[u8],
    describe: impl FnOnce() -> String,
) -> Result<(), FinalisedStateError> {
    match txn.put(db, &key, &value, WriteFlags::NO_OVERWRITE) {
        Ok(()) => Ok(()),
        Err(lmdb::Error::KeyExist) => {
            let existing = txn.get(db, &key).map_err(FinalisedStateError::LmdbError)?;
            if existing == value {
                Ok(())
            } else {
                Err(FinalisedStateError::Custom(describe()))
            }
        }
        Err(error) => Err(FinalisedStateError::LmdbError(error)),
    }
}

/// Flushes a buffered batch of `spent` index entries in sorted key order, then commits them
/// together with the Stage B progress watermark and fsyncs.
///
/// Sorting before insert turns the random-keyed `spent` B-tree fill into a sequential sweep rather
/// than a random fault per insert once the table exceeds RAM. Each flush is atomic and durable, so a
/// crash resumes from the last committed height; re-done work is idempotent (`NO_OVERWRITE` +
/// verify-match).
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
        put_idempotent(&mut txn, spent_db, outpoint_bytes, &entry_bytes, || {
            format!(
                "conflicting existing spent entry during batched migration for outpoint {}",
                hex::encode(outpoint_bytes)
            )
        })?;
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
        cfg: ChainIndexConfig,
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

        // Capability-gating during migration is handled by the orchestrator, which installs
        // an ephemeral passthrough so finalised reads are served from the source while the
        // indices below are (re)built; no per-capability toggle is needed here.

        // Use the persistent primary directly, not capability routing: the orchestrator has an
        // ephemeral passthrough installed for the migration's duration, and `backend(WriteCore)`
        // would route there (no LMDB env). The migration must write to the primary database.
        let backend = router.primary_backend();
        let env = backend.env()?;
        let metadata_db = backend.metadata_db()?;
        let txids_db = backend.txids_db()?;
        let transparent_db = backend.transparent_db()?;
        let spent_db = backend.spent_db()?;
        let txid_location_db = backend.txid_location_db()?;

        // Record that a migration is in progress (observability only; the migration resumes from
        // the per-stage progress keys below, not from `migration_status`).
        {
            let mut metadata: DbMetadata = backend.get_metadata().await?;
            if metadata.migration_status == MigrationStatus::Empty {
                metadata.migration_status = MigrationStatus::PartialBuidInProgress;
                backend.update_metadata(metadata).await?;
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
        if let Some(db_tip) = backend.db_height().await? {
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

                        // Idempotent on resume: an existing entry must match byte-for-byte, which
                        // also rejects a corrupt existing row (its checksum bytes would differ).
                        put_idempotent(
                            &mut txn,
                            txid_location_db,
                            txid_bytes,
                            &entry_bytes,
                            || {
                                format!(
                                "conflicting or corrupt existing txid_location entry at height {}",
                                height.0
                            )
                            },
                        )?;
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
            let batch_budget =
                (cfg.storage.database.sync_write_batch_size.to_byte_count() as u64).max(1);
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

                    for outpoint in transparent_tx.spent_outpoints() {
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
            backend.run_v1_2_migration_accumulator_stage(db_tip).await?;
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

        let mut metadata: DbMetadata = backend.get_metadata().await?;
        metadata.version = <Self as Migration<T>>::TO_VERSION;
        metadata.schema_hash =
            crate::chain_index::finalised_state::finalised_source::v1::DB_SCHEMA_V1_HASH;
        metadata.migration_status = MigrationStatus::Empty;
        backend.update_metadata(metadata).await?;
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

        info!("v1.1.0 to v1.2.0 migration complete.");
        Ok(())
    }
}

/// Patch migration: v1.2.0 → v1.2.1.
///
/// This is a **metadata-only** version marker. It records that the database was opened by a build
/// that supports optional ("ephemeral") finalised state and background (non-blocking) finalised-state
/// sync and migration. None of that behaviour changes the on-disk layout: the persisted tables, key
/// and value encodings, checksums, and `DB_SCHEMA_V1_HASH` are byte-for-byte identical to v1.2.0.
///
/// Because there is no data change, it uses the trait's default `migration_type` ([`MigrationType::Patch`])
/// and default `migrate` implementation, which only advances `DbMetadata::version` (and re-stamps the
/// unchanged schema checksum). It is idempotent, builds no shadow database, and rebuilds no indices.
struct Migration1_2_0To1_2_1;

impl<T: BlockchainSource> Migration<T> for Migration1_2_0To1_2_1 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 2,
        patch: 0,
    };

    const TO_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 2,
        patch: 1,
    };
}

/// Minor migration: v1.2.1 → v1.3.0.
///
/// Introduces the Ironwood (NU6.3) shielded pool to the finalised state:
/// - a new per-height `ironwood` table (`ironwood_1_3_0`), created empty by `DbV1::spawn`. New
///   blocks populate it via the write path; here it is backfilled for any stored block at or above
///   NU6.3 activation (see below).
/// - the `commitment_tree_data` table is rebuilt from the legacy fixed-length
///   `StoredEntryFixed<CommitmentTreeData>` (V1) rows into the variable-length
///   `StoredEntryVar<CommitmentTreeData>` (V2) layout, which carries the optional Ironwood root and
///   size. The new rows are written to `commitment_tree_data_1_3_0` (the primary's
///   `commitment_tree_data` handle), then the legacy table is cleared.
///
/// Per-height rebuild strategy, branching on NU6.3 activation:
/// - **Below activation:** rebuilt in place from the legacy on-disk commitment row (Ironwood
///   root/size default to `None`/`0` via `CommitmentTreeData` V1 decode). No validator access.
/// - **At or above activation:** the legacy data predates Ironwood, so both the commitment row
///   (now carrying the Ironwood root/size) and the sparse ironwood row are rebuilt from
///   validator-fetched block data via [`build_indexed_block_from_source`]. This lets the migration
///   run on a database already synced past NU6.3, rather than forcing a full re-index.
///
/// Safety and resumability:
/// - Deterministic: a below-activation row is derived only from its legacy row; an at/above-activation
///   row is refetched from immutable finalised history, so a resumed rebuild reproduces the same bytes.
/// - Resumable: the next height to rebuild is stored in the metadata DB under a temporary key.
/// - Crash-safe: each height's rebuilt rows and the progress update commit in the same transaction;
///   idempotent on resume (`NO_OVERWRITE` + verify-match).
struct Migration1_2_1To1_3_0;

impl<T: BlockchainSource> Migration<T> for Migration1_2_1To1_3_0 {
    const CURRENT_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 2,
        patch: 1,
    };

    const TO_VERSION: DbVersion = DbVersion {
        major: 1,
        minor: 3,
        patch: 0,
    };

    fn migration_type(&self) -> MigrationType {
        MigrationType::Minor
    }

    async fn migrate(
        &self,
        router: Arc<Router<T>>,
        cfg: ChainIndexConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        use lmdb::DatabaseFlags;

        use crate::chain_index::finalised_state::{
            build_indexed_block_from_source,
            finalised_source::v1::write_core::build_block_ironwood_entry, PoolActivationHeights,
        };

        // Temporary metadata entry recording the next height to rebuild, removed on completion.
        const MIGRATION_CTD_PROGRESS_KEY: &[u8] =
            b"_migration_commitment_tree_data_progress_1_3_0_next_height";

        info!("Starting v1.2.1 → v1.3.0 migration (Ironwood).");

        // Use the persistent primary directly (an ephemeral passthrough serves reads during the
        // migration; `backend(WriteCore)` would route there and has no LMDB env).
        let backend = router.primary_backend();
        let env = backend.env()?;
        let metadata_db = backend.metadata_db()?;
        // The primary's `commitment_tree_data` handle is the new `commitment_tree_data_1_3_0` table;
        // `ironwood_db` is the new (v1.3.0) sparse ironwood table backfilled below.
        let new_ctd_db = backend.commitment_tree_data_db()?;
        let ironwood_db = backend.ironwood_db()?;

        // Open the legacy fixed-length commitment table by name. On a pre-v1.3.0 database it already
        // exists; `open_or_create_db` creating it empty on an unexpected fresh DB is harmless (the
        // rebuild loop below only runs when a tip exists, and an empty legacy table yields no rows).
        let legacy_ctd_db =
            crate::chain_index::finalised_state::finalised_source::open_or_create_db(
                &env,
                "commitment_tree_data_1_0_0",
                DatabaseFlags::empty(),
            )
            .await?;

        // Network-upgrade activation heights (shared with `write_blocks_to_height`). Blocks at or
        // above NU6.3 have their ironwood root/size and ironwood tx list rebuilt from the validator
        // (the legacy on-disk data predates ironwood); blocks below it are rebuilt in place from
        // the legacy commitment row, with no ironwood.
        let network = cfg.network.clone();
        let pool_activations = PoolActivationHeights::resolve(&network);
        let sapling_activation_height = pool_activations.sapling;
        let nu5_activation_height = pool_activations.nu5;
        let nu6_3_activation_height = pool_activations.nu6_3;

        // Mark migration in progress (observability only; resumption uses the progress key).
        {
            let mut metadata: DbMetadata = backend.get_metadata().await?;
            if metadata.migration_status == MigrationStatus::Empty {
                metadata.migration_status = MigrationStatus::PartialBuidInProgress;
                backend.update_metadata(metadata).await?;
            }
        }

        // Reads the temporary progress height, returning `None` if the key is absent.
        let read_progress = |key: &[u8]| -> Result<Option<u32>, FinalisedStateError> {
            let txn = env.begin_ro_txn()?;
            match txn.get(metadata_db, &key) {
                Ok(bytes) => {
                    let entry = StoredEntryFixed::<Height>::from_bytes(bytes).map_err(|error| {
                        FinalisedStateError::Custom(format!(
                            "corrupt v1.3.0 migration progress entry: {error}"
                        ))
                    })?;
                    if !entry.verify(key) {
                        return Err(FinalisedStateError::Custom(
                            "v1.3.0 migration progress checksum mismatch".to_string(),
                        ));
                    }
                    Ok(Some(entry.inner().0))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(error) => Err(FinalisedStateError::LmdbError(error)),
            }
        };

        // Nothing to rebuild on an empty database; fall through to finalisation.
        if let Some(db_tip) = backend.db_height().await? {
            let db_tip = db_tip.0;

            let mut next_height =
                read_progress(MIGRATION_CTD_PROGRESS_KEY)?.unwrap_or(GENESIS_HEIGHT.0);

            info!(
                resume_height = next_height,
                db_tip,
                "v1.3.0 migration: rebuilding commitment_tree_data into StoredEntryVar (V2)"
            );
            let started = std::time::Instant::now();

            while next_height <= db_tip {
                let height = Height::try_from(next_height)
                    .map_err(|error| FinalisedStateError::Custom(error.to_string()))?;
                let height_bytes = height.to_bytes()?;

                let ironwood_active =
                    nu6_3_activation_height.is_some_and(|activation| next_height >= activation.0);

                // Prepare the rows to write. Reads / validator fetches happen before the write txn
                // so the commit stays short.
                let commitment_bytes: Vec<u8>;
                let ironwood_bytes: Option<Vec<u8>>;

                if ironwood_active {
                    // Post-NU6.3: the legacy row carries no ironwood root/size and the ironwood
                    // table has no row, so rebuild both from validator-fetched block data.
                    let sapling_activation_height = sapling_activation_height.ok_or_else(|| {
                        FinalisedStateError::Custom(
                            "Sapling activation height must be set to backfill ironwood"
                                .to_string(),
                        )
                    })?;
                    let block = build_indexed_block_from_source(
                        &source,
                        network.clone(),
                        sapling_activation_height,
                        nu5_activation_height,
                        nu6_3_activation_height,
                        next_height,
                        // Chainwork is irrelevant here: only the commitment-tree and ironwood rows
                        // are extracted, and neither depends on it.
                        None,
                    )
                    .await
                    .map_err(|error| {
                        FinalisedStateError::Custom(format!(
                            "v1.3.0 ironwood backfill failed at height {next_height}: {error}. \
                             This backfill refetches every stored block from NU6.3 activation \
                             through the database tip; ensure the backing validator serves that \
                             range, or wipe the finalised-state directory and re-index from the \
                             validator."
                        ))
                    })?;

                    commitment_bytes =
                        StoredEntryVar::new(&height_bytes, *block.commitment_tree_data())
                            .to_bytes()?;
                    ironwood_bytes = match build_block_ironwood_entry(&block, &height_bytes)? {
                        Some(entry) => Some(entry.to_bytes()?),
                        None => None,
                    };
                } else {
                    // Pre-NU6.3: rebuild the commitment row in place from the legacy fixed-length
                    // row (ironwood defaults to none).
                    let commitment_tree_data: CommitmentTreeData = {
                        let txn = env.begin_ro_txn()?;
                        let raw = txn
                            .get(legacy_ctd_db, &height_bytes)
                            .map_err(FinalisedStateError::LmdbError)?;
                        let entry = StoredEntryFixed::<CommitmentTreeData>::from_bytes(raw)
                            .map_err(|error| {
                                FinalisedStateError::Custom(format!(
                                    "legacy commitment_tree_data corrupt data: {error}"
                                ))
                            })?;
                        if !entry.verify(&height_bytes) {
                            return Err(FinalisedStateError::Custom(
                                "legacy commitment_tree_data checksum mismatch".to_string(),
                            ));
                        }
                        *entry.inner()
                    };
                    commitment_bytes =
                        StoredEntryVar::new(&height_bytes, commitment_tree_data).to_bytes()?;
                    ironwood_bytes = None;
                }

                // Write commitment (+ ironwood) and advance progress atomically.
                {
                    let mut txn = env.begin_rw_txn()?;
                    put_idempotent(
                        &mut txn,
                        new_ctd_db,
                        &height_bytes,
                        &commitment_bytes,
                        || {
                            format!(
                                "conflicting rebuilt commitment_tree_data at height {}",
                                height.0
                            )
                        },
                    )?;
                    if let Some(bytes) = &ironwood_bytes {
                        put_idempotent(&mut txn, ironwood_db, &height_bytes, bytes, || {
                            format!("conflicting rebuilt ironwood at height {}", height.0)
                        })?;
                    }

                    let progress = StoredEntryFixed::new(MIGRATION_CTD_PROGRESS_KEY, height + 1);
                    txn.put(
                        metadata_db,
                        &MIGRATION_CTD_PROGRESS_KEY,
                        &progress.to_bytes()?,
                        WriteFlags::empty(),
                    )?;
                    txn.commit()?;
                }

                if next_height % SYNC_CHECKPOINT_INTERVAL == 0 {
                    env.sync(true)?;
                }
                if next_height % 50_000 == 0 {
                    info!(
                        height = next_height,
                        db_tip,
                        elapsed = ?started.elapsed(),
                        "v1.3.0 migration progress"
                    );
                }

                next_height = height.0 + 1;
            }

            env.sync(true)?;
            info!(
                db_tip,
                elapsed = ?started.elapsed(),
                "v1.3.0 migration: commitment_tree_data rebuild complete"
            );

            // Drop the legacy commitment data: clearing frees its pages back to the env freelist.
            {
                let mut txn = env.begin_rw_txn()?;
                txn.clear_db(legacy_ctd_db)?;
                txn.commit()?;
            }
            env.sync(true)?;
        }

        // Finalise: advance the version durably (the completion gate) before removing the progress
        // key, so a crash re-selects and cheaply resumes this migration rather than skipping it.
        env.sync(true)?;
        let mut metadata: DbMetadata = backend.get_metadata().await?;
        metadata.version = <Self as Migration<T>>::TO_VERSION;
        metadata.schema_hash =
            crate::chain_index::finalised_state::finalised_source::v1::DB_SCHEMA_V1_HASH;
        metadata.migration_status = MigrationStatus::Empty;
        backend.update_metadata(metadata).await?;
        env.sync(true)?;

        {
            let mut txn = env.begin_rw_txn()?;
            match txn.del(metadata_db, &MIGRATION_CTD_PROGRESS_KEY, None) {
                Ok(()) | Err(lmdb::Error::NotFound) => {}
                Err(error) => return Err(FinalisedStateError::LmdbError(error)),
            }
            txn.commit()?;
        }
        env.sync(true)?;

        info!("v1.2.1 to v1.3.0 migration complete.");
        Ok(())
    }
}
