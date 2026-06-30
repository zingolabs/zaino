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
//!   - plans a path over the version graph via [`plan_migrations`] and runs each step in order.
//!
//! - [`MigrationStep`] and the [`migrations!`] macro:
//!   - the macro generates the `MigrationStep` dispatch enum (static, no `dyn`) and the
//!     `MigrationStep::all` registry from a single authored list; [`plan_migrations`] walks that
//!     registry as a graph (nodes = `DbVersion`, edges = steps) to find the path to the target.
//!
//! The authoritative spec for this module is ADR 0002
//! (`docs/adr/0002-persistent-finalised-state-migrations.md`).
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
//! See ADR 0002's "Engineer / dev guide" for the full recipe. In short:
//!
//! 1. Introduce a new `#[derive(Clone, Copy)] struct MigrationX_Y_ZToA_B_C;` and implement
//!    `Migration<T>` (set `CURRENT_VERSION` / `TO_VERSION`, and `migration_type()` for non-patch).
//! 2. Add its name to the `migrations! { .. }` list — that registers it for the planner; there is no
//!    strict `(major, minor, patch)` match to edit.
//! 3. Update the version constants in the versions module so the registry edge and
//!    `latest_version_for_major` agree.
//! 4. Ensure the migration is deterministic, resumable (`DbMetadata::migration_status` and/or the
//!    target backend's own tip), and crash-safe (never leaves a partially promoted DB).
//! 5. Add tests/fixtures: starting from the old version, resuming mid-build if applicable, and
//!    validating the resulting DB serves the required capabilities.
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
    Height, TransparentTxList, TxLocation, TxidList, ZainoVersionedSerde as _,
};

use lmdb::{Transaction, WriteFlags};

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
/// Note: concrete steps are selected by [`plan_migrations`] from the [`migrations!`] registry; this
/// enum drives the per-type lifecycle in [`MigrationManager::migrate`].
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
    // `from_version` is a getter for the source version (per ADR 0002), not a constructor.
    #[allow(clippy::wrong_self_convention)]
    fn from_version(&self) -> DbVersion {
        Self::CURRENT_VERSION
    }

    /// Returns the version this step migrates *to*.
    fn to_version(&self) -> DbVersion {
        Self::TO_VERSION
    }

    /// Returns the routing/lifecycle category for this migration, which selects both the manager's
    /// plumbing (see [`MigrationManager::migrate`]) and the default [`Migration::migrate`] body:
    ///
    /// - **Patch** — runs directly against the primary; the default `migrate` advances `DbMetadata`
    ///   only.
    /// - **Minor** — runs while the manager holds a full-mode ephemeral reference (reads served from
    ///   the validator while the primary is rebuilt in place); the migration **must** override
    ///   `migrate`.
    /// - **Major** — the default `migrate` builds the new major from the validator and promotes it
    ///   (see [`build_and_promote_major`]); the old primary keeps serving until the swap, so the
    ///   manager installs no full-duration ephemeral.
    fn migration_type(&self) -> MigrationType {
        MigrationType::Patch
    }

    /// Performs the migration step.
    ///
    /// The default dispatches on [`Migration::migration_type`]:
    /// - **Patch** → a metadata-only advance of the recorded version ([`migrate_metadata_only`]).
    /// - **Major** → the generic build-and-promote helper ([`build_and_promote_major`]), targeting
    ///   `TO_VERSION` (the newest version of the new major).
    /// - **Minor** → fails fast: a minor migration carries bespoke in-place rebuild logic and **must**
    ///   override this method. A compile-time check is not expressible against the runtime
    ///   `migration_type` discriminant, so this is a fail-fast guard (covered by a registry test).
    ///
    /// # Errors
    /// Returns `FinalisedStateError` if the migration cannot proceed safely or deterministically.
    async fn migrate(
        &self,
        router: Arc<Router<T>>,
        cfg: ChainIndexConfig,
        source: T,
    ) -> Result<(), FinalisedStateError> {
        match self.migration_type() {
            MigrationType::Patch => {
                migrate_metadata_only::<T>(router, Self::CURRENT_VERSION, Self::TO_VERSION).await
            }
            MigrationType::Major => {
                build_and_promote_major::<T>(router, cfg, source, Self::TO_VERSION).await
            }
            MigrationType::Minor => Err(FinalisedStateError::Custom(format!(
                "minor migration {} -> {} must override migrate()",
                Self::CURRENT_VERSION,
                Self::TO_VERSION,
            ))),
        }
    }
}

/// Metadata-only migration: advances the recorded `DbMetadata::version` (and re-stamps the schema
/// checksum), touching no table data. This is the default behaviour of a **patch** migration.
async fn migrate_metadata_only<T: BlockchainSource>(
    router: Arc<Router<T>>,
    from: DbVersion,
    to: DbVersion,
) -> Result<(), FinalisedStateError> {
    info!("Starting metadata-only migration from {from} to {to}.");

    let mut metadata: DbMetadata = router.get_metadata().await?;
    metadata.version = to;
    metadata.schema_hash =
        crate::chain_index::finalised_state::finalised_source::v1::DB_SCHEMA_V1_HASH;
    metadata.migration_status = MigrationStatus::Empty;
    router.update_metadata(metadata).await?;

    info!("Metadata-only migration from {from} to {to} complete.");
    Ok(())
}

/// Generic **major** migration: builds the target major from the backing validator and promotes it.
///
/// This is the default behaviour of a major migration (overridable for a bespoke major). The old
/// primary keeps serving read+write while the new backend is built in its own directory; a brief
/// ephemeral freeze covers the final catch-up and the atomic primary swap; the old directory is then
/// kept or deleted per the retention policy. `target` is the newest version of the new major.
///
/// Implemented in a later step (it depends on `FinalisedSource::spawn_major`, the retention config,
/// and `Router::replace_primary`). No major migration is registered yet, so this path is currently
/// unreachable.
async fn build_and_promote_major<T: BlockchainSource>(
    _router: Arc<Router<T>>,
    _cfg: ChainIndexConfig,
    _source: T,
    target: DbVersion,
) -> Result<(), FinalisedStateError> {
    Err(FinalisedStateError::Custom(format!(
        "major build-and-promote to {target} is not yet implemented"
    )))
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
    /// Plans a path from `current_version` to `target_version` over the migration version graph,
    /// then performs each step in order.
    ///
    /// The plan is computed once via [`plan_migrations`]; each step maps one specific `DbVersion`
    /// to the next, and `current_version` is advanced to each step's `to_version` as it completes.
    ///
    /// # Errors
    /// Returns an error if no path exists from the current version to the target, or if any
    /// migration step fails.
    pub(super) async fn migrate(&mut self) -> Result<(), FinalisedStateError> {
        let plan = plan_migrations::<T>(self.current_version, self.target_version)?;

        for step in plan {
            match step.migration_type::<T>() {
                // Patch: metadata-only, runs directly against the primary.
                MigrationType::Patch => {
                    step.migrate(
                        Arc::clone(&self.router),
                        self.cfg.clone(),
                        self.source.clone(),
                    )
                    .await?;
                }

                // Minor: in-place rebuild of the one primary. Hold a full-mode ephemeral reference
                // for the duration so reads are served from the validator while the primary is
                // rewritten.
                MigrationType::Minor => {
                    let primary = self.router.primary_backend();
                    let db_height = primary.db_height().await?;

                    let _ephemeral_reference = self
                        .router
                        .init_or_take_ephemeral(
                            self.source.clone(),
                            self.cfg.network.to_zebra_network(),
                            EphemeralMode::Full,
                            db_height,
                        )
                        .await?;

                    step.migrate(
                        Arc::clone(&self.router),
                        self.cfg.clone(),
                        self.source.clone(),
                    )
                    .await?;
                }

                // Major: the old primary keeps serving while a new major is built in its own
                // directory; the build-and-promote helper installs its own brief ephemeral freeze
                // for the swap, so the manager holds no full-duration ephemeral here.
                MigrationType::Major => {
                    step.migrate(
                        Arc::clone(&self.router),
                        self.cfg.clone(),
                        self.source.clone(),
                    )
                    .await?;
                }
            }

            self.current_version = step.to_version::<T>();
        }

        Ok(())
    }
}

/// Generates the [`MigrationStep`] dispatch enum and the planner's registry from a single authored
/// list of migration types.
///
/// Each listed type is a unit struct implementing [`Migration<T>`]. The macro generates:
/// - the `MigrationStep` enum (one variant per type),
/// - static dispatch for `from_version` / `to_version` / `migration_type` / `migrate` (no `dyn`),
/// - `MigrationStep::all`, the registry [`plan_migrations`] walks to build the version graph.
///
/// Adding a migration is therefore: implement `Migration<T>` for a new unit struct, then add its
/// name to the `migrations! { .. }` list below. A `fn` cannot generate enum variants or match arms,
/// so a macro is the minimal tool for this (per the repo's DRY guideline).
macro_rules! migrations {
    ($($step:ident),* $(,)?) => {
        /// Concrete migration step selector (generated by [`migrations!`]).
        ///
        /// Rust cannot return `impl Migration<T>` from a `match` that selects between multiple
        /// concrete migration types, so this enum provides the static dispatch over them.
        #[derive(Clone, Copy)]
        enum MigrationStep {
            $($step($step),)*
        }

        impl MigrationStep {
            /// Every registered migration step, in registry order. The planner builds the version
            /// graph (nodes = `DbVersion`, edges = these steps) from this list.
            fn all() -> Vec<MigrationStep> {
                vec![$(MigrationStep::$step($step),)*]
            }

            /// The exact on-disk version this step migrates *from*.
            // Getters for the source/target versions (per ADR 0002), not constructors; the
            // `from_*`/`to_*` self-convention lints don't apply.
            #[allow(clippy::wrong_self_convention)]
            fn from_version<T: BlockchainSource>(&self) -> DbVersion {
                match self {
                    $(MigrationStep::$step(_) => <$step as Migration<T>>::CURRENT_VERSION,)*
                }
            }

            /// The exact on-disk version this step migrates *to*.
            #[allow(clippy::wrong_self_convention)]
            fn to_version<T: BlockchainSource>(&self) -> DbVersion {
                match self {
                    $(MigrationStep::$step(_) => <$step as Migration<T>>::TO_VERSION,)*
                }
            }

            /// The routing/lifecycle category for this step.
            fn migration_type<T: BlockchainSource>(&self) -> MigrationType {
                match self {
                    $(MigrationStep::$step(step) => <$step as Migration<T>>::migration_type(step),)*
                }
            }

            /// Runs this step.
            async fn migrate<T: BlockchainSource>(
                &self,
                router: Arc<Router<T>>,
                cfg: ChainIndexConfig,
                source: T,
            ) -> Result<(), FinalisedStateError> {
                match self {
                    $(MigrationStep::$step(step) => step.migrate(router, cfg, source).await,)*
                }
            }
        }
    };
}

migrations! {
    Migration1_0_0To1_1_0,
    Migration1_1_0To1_2_0,
    Migration1_2_0To1_2_1,
}

/// Computes the ordered sequence of migration steps from `current` to `target` over the version
/// graph formed by the registered migrations (nodes = `DbVersion`, edges = `MigrationStep`s).
///
/// The path is the shortest one found by breadth-first search; ties are broken by registry order,
/// so the result is deterministic. Returns an empty plan when `current == target`.
///
/// # Errors
/// Returns [`FinalisedStateError::Custom`] if no path exists from `current` to `target`.
fn plan_migrations<T: BlockchainSource>(
    current: DbVersion,
    target: DbVersion,
) -> Result<Vec<MigrationStep>, FinalisedStateError> {
    use std::collections::{HashMap, HashSet, VecDeque};

    if current == target {
        return Ok(Vec::new());
    }

    let steps = MigrationStep::all();

    // Adjacency by source version, preserving registry order so ties break deterministically.
    let mut adjacency: HashMap<DbVersion, Vec<usize>> = HashMap::new();
    for (index, step) in steps.iter().enumerate() {
        adjacency
            .entry(step.from_version::<T>())
            .or_default()
            .push(index);
    }

    // BFS from `current`, recording the predecessor edge for each newly reached version.
    let mut predecessor: HashMap<DbVersion, (DbVersion, usize)> = HashMap::new();
    let mut visited: HashSet<DbVersion> = HashSet::from([current]);
    let mut queue: VecDeque<DbVersion> = VecDeque::from([current]);

    while let Some(version) = queue.pop_front() {
        if version == target {
            break;
        }
        if let Some(indices) = adjacency.get(&version) {
            for &index in indices {
                let next = steps[index].to_version::<T>();
                if visited.insert(next) {
                    predecessor.insert(next, (version, index));
                    queue.push_back(next);
                }
            }
        }
    }

    if !visited.contains(&target) {
        return Err(FinalisedStateError::Custom(format!(
            "no migration path from {current} to {target}"
        )));
    }

    // Walk predecessors back from `target` to `current`, then reverse into forward order.
    let mut plan = Vec::new();
    let mut cursor = target;
    while cursor != current {
        let (previous, index) = *predecessor.get(&cursor).ok_or_else(|| {
            FinalisedStateError::Custom(
                "internal error: incomplete migration path reconstruction".to_string(),
            )
        })?;
        plan.push(steps[index]);
        cursor = previous;
    }
    plan.reverse();

    Ok(plan)
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
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
struct Migration1_1_0To1_2_0;

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
#[derive(Clone, Copy)]
struct Migration1_2_0To1_2_1;

#[async_trait]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::source::mockchain_source::MockchainSource;

    // Type witness: `plan_migrations` only reads the version constants, which are identical for
    // every `BlockchainSource`, so any concrete source type works to instantiate it.
    type Source = MockchainSource;

    fn v(major: u32, minor: u32, patch: u32) -> DbVersion {
        DbVersion::new(major, minor, patch)
    }

    /// Flattens a plan into its `(from, to)` edges for assertion.
    fn edges(plan: &[MigrationStep]) -> Vec<(DbVersion, DbVersion)> {
        plan.iter()
            .map(|step| (step.from_version::<Source>(), step.to_version::<Source>()))
            .collect()
    }

    #[test]
    fn registry_edges_match_authored_list() {
        assert_eq!(
            edges(&MigrationStep::all()),
            vec![
                (v(1, 0, 0), v(1, 1, 0)),
                (v(1, 1, 0), v(1, 2, 0)),
                (v(1, 2, 0), v(1, 2, 1)),
            ],
        );
    }

    #[test]
    fn plans_full_linear_path() {
        let plan = plan_migrations::<Source>(v(1, 0, 0), v(1, 2, 1)).expect("path exists");
        assert_eq!(
            edges(&plan),
            vec![
                (v(1, 0, 0), v(1, 1, 0)),
                (v(1, 1, 0), v(1, 2, 0)),
                (v(1, 2, 0), v(1, 2, 1)),
            ],
        );
    }

    #[test]
    fn plans_partial_path() {
        let plan = plan_migrations::<Source>(v(1, 1, 0), v(1, 2, 0)).expect("path exists");
        assert_eq!(edges(&plan), vec![(v(1, 1, 0), v(1, 2, 0))]);
    }

    #[test]
    fn plan_for_equal_versions_is_empty() {
        let plan = plan_migrations::<Source>(v(1, 2, 1), v(1, 2, 1)).expect("trivially reachable");
        assert!(plan.is_empty());
    }

    #[test]
    fn no_path_to_unregistered_target_errors() {
        // No edge reaches a 2.x major yet, so the target is unreachable.
        assert!(plan_migrations::<Source>(v(1, 0, 0), v(2, 0, 0)).is_err());
    }

    #[test]
    fn no_path_from_unregistered_source_errors() {
        // No edge departs from 0.0.0 (legacy v0 is rejected, not migrated).
        assert!(plan_migrations::<Source>(v(0, 0, 0), v(1, 1, 0)).is_err());
    }
}
